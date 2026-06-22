//! # Supervision-level Prometheus metrics.
//!
//! [`PrometheusSubscriber`] implements [`Subscribe`] and translates [`taskvisor`] events into Prometheus counters, gauges, and histograms.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::sync::Arc;

use prometheus::{Counter, CounterVec, Gauge, Histogram, HistogramVec, Registry};
use taskvisor::{BackoffSource, Event, EventKind, Subscribe};

use crate::register::{Sub, ms_to_secs};

/// Default subscriber queue capacity.
pub const DEFAULT_QUEUE_CAPACITY: usize = 2048;

/// Prometheus subscriber for supervision-level metrics.
///
/// Implements [`Subscribe`] and captures metrics from the [`taskvisor`] event stream.
/// Must share a [`Registry`] with [`crate::PrometheusMetrics`] for a unified `/metrics` endpoint.
///
/// ## Event → metric mapping
///
/// ```text
/// TaskStarting        → tasks_in_flight.inc()
///                       + task_restarts.inc()  (if attempt > 1)
/// TaskStopped         → tasks_in_flight.dec()
/// TaskFailed          → tasks_in_flight.dec()
/// TimeoutHit          → task_timeouts.inc()
/// BackoffScheduled    → task_backoff_count{source}.inc()
///                       + task_backoff_duration.observe(delay)
/// ActorExhausted      → task_terminal{reason}.inc()   (reason="completed" if the reason is policy_exhausted_success, else "exhausted")
///                       + attempts_to_finalize{outcome}.observe(attempt)
/// ActorDead           → task_terminal{reason="fatal"}.inc()
///                       + attempts_to_finalize{outcome="fatal"}.observe(attempt)
/// SubscriberOverflow  → subscriber_overflow.inc() + tracing::warn
/// SubscriberPanicked  → subscriber_panicked.inc() + tracing::warn
/// ControllerSubmitted → controller_submissions.inc()
/// ControllerRejected  → controller_rejections{reason}.inc()  (reason classified from Event.reason)
/// ```
///
/// ## Supervision metrics (`solti_sv_*`)
///
/// | Metric                                   | Type      | Labels   | Description                  |
/// |------------------------------------------|-----------|----------|------------------------------|
/// | `solti_sv_tasks_in_flight`               | Gauge     | -        | Currently executing tasks    |
/// | `solti_sv_task_restarts_total`           | Counter   | -        | Restarts (attempt > 1)       |
/// | `solti_sv_task_backoff_count_total`      | Counter   | `source` | Backoff events               |
/// | `solti_sv_task_backoff_duration_seconds` | Histogram | -        | Backoff delay duration       |
/// | `solti_sv_task_terminal_total`           | Counter   | `reason` | Terminal task states         |
/// | `solti_sv_attempts_to_finalize`          | Histogram | `outcome`| Attempts when task left loop |
/// | `solti_sv_task_timeouts_total`           | Counter   | -        | Timeout events               |
/// | `solti_sv_subscriber_overflow_total`     | Counter   | -        | Queue overflow (lost events) |
/// | `solti_sv_subscriber_panicked_total`     | Counter   | -        | Subscriber panics            |
///
/// ## Controller metrics (`solti_ctrl_*`)
///
/// | Metric                         | Type      | Labels   | Description                             |
/// |--------------------------------|-----------|----------|-----------------------------------------|
/// | `solti_ctrl_submissions_total` | Counter   | -        | Controller submissions                  |
/// | `solti_ctrl_rejections_total`  | CounterVec| `reason` | Controller rejections grouped by cause  |
///
/// ## Labels
///
/// | Label    | Values                                                                                                                           | Source                                                       |
/// |----------|----------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------|
/// | `source` | `failure`, `success`                                                                                                             | [`BackoffSource`] on the event                               |
/// | `reason` (terminal)   | `completed`, `exhausted`, `fatal`                                                                                   | Terminal event kind (`completed` = policy_exhausted_success) |
/// | `outcome` (attempts)  | `completed`, `exhausted`, `fatal`                                                                                   | Attempts-to-finalize histogram                               |
/// | `reason` (rejection)  | `slot_full`, `slot_busy`, `superseded`, `removed`, `shutting_down`, `add_failed`, `remove_failed`, `queue_failed`, `recovery_failed`, `bus_lagged`, `controller_exited`, `other`, `unknown` | Classified from `Event.reason` by `classify_rejection_reason` (private)         |
///
/// ## Notes
///
/// - `tasks_in_flight` is **best-effort**. It is derived from taskvisor's lossy
///   broadcast bus (inc on `TaskStarting`, dec on the per-attempt terminal), so a
///   dropped event under sustained bus lag makes it drift and it does not
///   self-heal. It is guarded against going negative (a terminal without a
///   preceding start is a no-op). For an **authoritative**, self-correcting count
///   that is recomputed from `TaskState` on every scrape, use the pull-based
///   `PrometheusStateCollector` (`state` feature): `solti_sv_tasks_by_phase{phase="running"}`.
/// - [`queue_capacity`](Subscribe::queue_capacity) defaults to [`DEFAULT_QUEUE_CAPACITY`].
/// - Backoff duration is converted from milliseconds to seconds before observation.
///
/// ## Also
///
/// - [`PrometheusMetrics`](crate::PrometheusMetrics) is a runner-level metrics, complementary to this subscriber.
/// - [`Event`](taskvisor::Event) and [`EventKind`](taskvisor::EventKind): event structure and classification.
pub struct PrometheusSubscriber {
    tasks_in_flight: Gauge,
    task_restarts: Counter,
    task_backoff_count: CounterVec,
    task_backoff_duration: Histogram,
    task_terminal: CounterVec,
    attempts_to_finalize: HistogramVec,
    task_timeouts: Counter,
    subscriber_overflow: Counter,
    subscriber_panicked: Counter,
    controller_submissions: Counter,
    controller_rejections: CounterVec,
    queue_capacity: usize,
}

/// Map a free-form `ControllerRejected` reason string to a bounded, low-cardinality metric label.
///
/// The raw `reason` on [`taskvisor::Event`] can embed error messages, queue depths, and other unbounded content, which would explode Prometheus cardinality if used directly as a label.
/// This classifier collapses known prefixes produced by taskvisor's controller into a small set:
///
/// [`slot_full`, `slot_busy`, `superseded`, `removed`, `shutting_down`, `add_failed`, `remove_failed`, `queue_failed`, `recovery_failed`, `bus_lagged`, `controller_exited`, `other`, `unknown` ].
fn classify_rejection_reason(reason: Option<&str>) -> &'static str {
    let Some(r) = reason else {
        return "unknown";
    };
    if r.starts_with("queue_full") {
        "slot_full"
    } else if r.starts_with("dropped: slot busy") {
        "slot_busy"
    } else if r.starts_with("superseded_by_replace") {
        "superseded"
    } else if r.starts_with("removed_from_queue") {
        "removed"
    } else if r.starts_with("controller_shutting_down") {
        "shutting_down"
    } else if r.starts_with("add_failed") {
        "add_failed"
    } else if r.starts_with("remove_failed") {
        "remove_failed"
    } else if r.starts_with("queue_start_failed") {
        "queue_failed"
    } else if r.starts_with("recovery_remove_failed") {
        "recovery_failed"
    } else if r.starts_with("bus_lagged") {
        "bus_lagged"
    } else if r.starts_with("controller_loop_exited") {
        "controller_exited"
    } else {
        "other"
    }
}

impl PrometheusSubscriber {
    /// Create a new subscriber with the default event-bus queue capacity ([`DEFAULT_QUEUE_CAPACITY`]).
    pub fn new(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        Self::with_queue_capacity(registry, DEFAULT_QUEUE_CAPACITY)
    }

    /// Create a new subscriber with a specific event-bus queue capacity.
    pub fn with_queue_capacity(
        registry: Arc<Registry>,
        queue_capacity: usize,
    ) -> Result<Self, prometheus::Error> {
        let sv = Sub::new(&registry, "sv");
        let ctrl = Sub::new(&registry, "ctrl");

        let tasks_in_flight = sv.gauge("tasks_in_flight", "Number of tasks currently executing")?;
        let task_restarts =
            sv.counter("task_restarts_total", "Total task restarts (attempt > 1)")?;
        let task_backoff_count = sv.counter_vec(
            "task_backoff_count_total",
            "Total backoff events",
            &["source"],
        )?;
        let task_backoff_duration = sv.histogram(
            "task_backoff_duration_seconds",
            "Backoff delay duration in seconds",
            vec![
                0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
                3600.0,
            ],
        )?;
        let task_terminal = sv.counter_vec(
            "task_terminal_total",
            "Total terminal task states",
            &["reason"],
        )?;
        let attempts_to_finalize = sv.histogram_vec(
            "attempts_to_finalize",
            "Number of attempts observed when a task leaves the supervision loop",
            vec![1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0],
            &["outcome"],
        )?;
        let task_timeouts = sv.counter("task_timeouts_total", "Total task timeout events")?;
        let subscriber_overflow = sv.counter(
            "subscriber_overflow_total",
            "Total subscriber queue overflow events (events lost)",
        )?;
        let subscriber_panicked =
            sv.counter("subscriber_panicked_total", "Total subscriber panic events")?;

        let controller_submissions =
            ctrl.counter("submissions_total", "Total controller submissions")?;
        let controller_rejections = ctrl.counter_vec(
            "rejections_total",
            "Total controller rejections grouped by cause",
            &["reason"],
        )?;

        Ok(Self {
            tasks_in_flight,
            task_restarts,
            task_backoff_count,
            task_backoff_duration,
            task_terminal,
            attempts_to_finalize,
            task_timeouts,
            subscriber_overflow,
            subscriber_panicked,
            controller_submissions,
            controller_rejections,
            queue_capacity,
        })
    }
}

impl std::fmt::Debug for PrometheusSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusSubscriber").finish()
    }
}

impl Subscribe for PrometheusSubscriber {
    /// Translates a [`taskvisor`] event into prometheus metric updates.
    fn on_event(&self, event: &Event) {
        match event.kind {
            EventKind::TaskStarting => {
                self.tasks_in_flight.inc();
                if event.attempt.unwrap_or(1) > 1 {
                    self.task_restarts.inc();
                }
            }
            EventKind::TaskStopped | EventKind::TaskCanceled | EventKind::TaskFailed => {
                if self.tasks_in_flight.get() > 0.0 {
                    self.tasks_in_flight.dec();
                }
            }
            EventKind::TimeoutHit => {
                self.task_timeouts.inc();
            }
            EventKind::SubscriberOverflow => {
                tracing::warn!(
                    task = event.task.as_deref().unwrap_or("unknown"),
                    "subscriber queue overflow: events are being dropped"
                );
                self.subscriber_overflow.inc();
            }
            EventKind::SubscriberPanicked => {
                tracing::warn!(
                    task = event.task.as_deref().unwrap_or("unknown"),
                    reason = event.reason.as_deref().unwrap_or("unknown"),
                    "subscriber panicked while processing an event"
                );
                self.subscriber_panicked.inc();
            }
            EventKind::BackoffScheduled => {
                let source = match event.backoff_source {
                    Some(BackoffSource::Failure) => "failure",
                    Some(BackoffSource::Success) => "success",
                    None => "unknown",
                };
                self.task_backoff_count.with_label_values(&[source]).inc();

                if let Some(delay_ms) = event.delay_ms {
                    self.task_backoff_duration
                        .observe(ms_to_secs(delay_ms.into()));
                }
            }
            EventKind::ActorExhausted => {
                // Success under OnFailure/Never is the normal way a task ends,
                // not retry exhaustion: keep the two distinguishable.
                // NB: the literal mirrors `solti_core::reasons::POLICY_EXHAUSTED_SUCCESS`
                // (the canonical, CI-pinned list); solti-prometheus does not depend
                // on solti-core unconditionally, so it is duplicated here.
                let label = if event.reason.as_deref() == Some("policy_exhausted_success") {
                    "completed"
                } else {
                    "exhausted"
                };
                self.task_terminal.with_label_values(&[label]).inc();
                self.attempts_to_finalize
                    .with_label_values(&[label])
                    .observe(f64::from(event.attempt.unwrap_or(1)));
            }
            EventKind::ActorDead => {
                self.task_terminal.with_label_values(&["fatal"]).inc();
                self.attempts_to_finalize
                    .with_label_values(&["fatal"])
                    .observe(f64::from(event.attempt.unwrap_or(1)));
            }
            EventKind::ControllerSubmitted => {
                self.controller_submissions.inc();
            }
            EventKind::ControllerRejected => {
                let reason = classify_rejection_reason(event.reason.as_deref());
                self.controller_rejections
                    .with_label_values(&[reason])
                    .inc();
            }
            // A force-aborted attempt never publishes its own terminal event:
            // compensate the in-flight gauge from the removal notification.
            EventKind::TaskRemoved => {
                if event.reason.as_deref() == Some("force_terminated_after_grace")
                    && self.tasks_in_flight.get() > 0.0
                {
                    self.tasks_in_flight.dec();
                }
            }
            EventKind::TaskAdded
            | EventKind::TaskAddFailed
            | EventKind::TaskAddRequested
            | EventKind::TaskRemoveRequested
            | EventKind::ShutdownRequested
            | EventKind::AllStoppedWithinGrace
            | EventKind::GraceExceeded
            | EventKind::ControllerSlotTransition => {}

            // `EventKind` is #[non_exhaustive]: ignore variants added in future taskvisor releases.
            _ => {}
        }
    }

    /// Returns `"prometheus"`
    fn name(&self) -> &'static str {
        "prometheus"
    }

    /// Returns the per-subscriber queue capacity configured via [`PrometheusSubscriber::new`] or [`PrometheusSubscriber::with_queue_capacity`].
    fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Encoder;
    use solti_runner::MetricsBackend;
    use std::time::Duration;

    fn new_subscriber() -> PrometheusSubscriber {
        let registry = Arc::new(Registry::new());
        PrometheusSubscriber::new(registry).unwrap()
    }

    fn metrics_text(registry: &Registry) -> String {
        let encoder = prometheus::TextEncoder::new();
        let families = registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn task_starting_increments_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );

        assert_eq!(sub.tasks_in_flight.get(), 1.0);
    }

    #[test]
    fn task_stopped_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(&Event::new(EventKind::TaskStopped).with_task("t"));

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn task_canceled_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(&Event::new(EventKind::TaskCanceled).with_task("t"));

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn force_terminated_removal_decrements_in_flight() {
        let sub = new_subscriber();

        // A force-aborted attempt never publishes its own terminal event:
        // the only signal is TaskRemoved("force_terminated_after_grace").
        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::TaskRemoved)
                .with_task("t")
                .with_reason("force_terminated_after_grace"),
        );

        assert_eq!(
            sub.tasks_in_flight.get(),
            0.0,
            "force-terminated tasks must not leak the in-flight gauge"
        );
    }

    #[test]
    fn exhausted_after_success_is_labelled_completed() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("t")
                .with_attempt(1)
                .with_reason("policy_exhausted_success"),
        );
        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("t2")
                .with_attempt(5)
                .with_reason("max_retries_exceeded(5/5): boom"),
        );

        assert_eq!(
            sub.task_terminal.with_label_values(&["completed"]).get(),
            1.0,
            "normal one-shot completion must not be counted as exhaustion"
        );
        assert_eq!(
            sub.task_terminal.with_label_values(&["exhausted"]).get(),
            1.0
        );
    }

    #[test]
    fn classifier_recognizes_taskvisor_03_reasons() {
        assert_eq!(
            classify_rejection_reason(Some("superseded_by_replace")),
            "superseded"
        );
        assert_eq!(
            classify_rejection_reason(Some("removed_from_queue")),
            "removed"
        );
        assert_eq!(
            classify_rejection_reason(Some("controller_shutting_down")),
            "shutting_down"
        );
    }

    #[test]
    fn task_failed_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::TaskFailed)
                .with_task("t")
                .with_reason("boom"),
        );

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn first_attempt_is_not_a_restart() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );

        assert_eq!(sub.task_restarts.get(), 0.0);
    }

    #[test]
    fn second_attempt_is_a_restart() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(2),
        );

        assert_eq!(sub.task_restarts.get(), 1.0);
    }

    #[test]
    fn backoff_failure_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::BackoffScheduled)
                .with_task("t")
                .with_delay(Duration::from_secs(5))
                .with_backoff_failure(),
        );

        assert_eq!(
            sub.task_backoff_count.with_label_values(&["failure"]).get(),
            1.0
        );
    }

    #[test]
    fn backoff_success_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::BackoffScheduled)
                .with_task("t")
                .with_delay(Duration::from_secs(10))
                .with_backoff_success(),
        );

        assert_eq!(
            sub.task_backoff_count.with_label_values(&["success"]).get(),
            1.0
        );
    }

    #[test]
    fn timeout_hit_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TimeoutHit)
                .with_task("t")
                .with_timeout(Duration::from_secs(30)),
        );

        assert_eq!(sub.task_timeouts.get(), 1.0);
    }

    #[test]
    fn actor_exhausted_increments_terminal() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("t")
                .with_reason("policy done"),
        );

        assert_eq!(
            sub.task_terminal.with_label_values(&["exhausted"]).get(),
            1.0
        );
    }

    #[test]
    fn actor_exhausted_observes_attempts_to_finalize() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("t")
                .with_attempt(3),
        );

        let h = sub.attempts_to_finalize.with_label_values(&["exhausted"]);
        assert_eq!(h.get_sample_count(), 1);
        assert_eq!(h.get_sample_sum(), 3.0);
    }

    #[test]
    fn actor_dead_observes_attempts_to_finalize() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorDead)
                .with_task("t")
                .with_attempt(7)
                .with_reason("fatal"),
        );

        let h = sub.attempts_to_finalize.with_label_values(&["fatal"]);
        assert_eq!(h.get_sample_count(), 1);
        assert_eq!(h.get_sample_sum(), 7.0);
    }

    #[test]
    fn actor_exhausted_without_attempt_observes_one() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::ActorExhausted).with_task("t"));

        let h = sub.attempts_to_finalize.with_label_values(&["exhausted"]);
        assert_eq!(h.get_sample_count(), 1);
        assert_eq!(h.get_sample_sum(), 1.0);
    }

    #[test]
    fn actor_dead_increments_terminal() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorDead)
                .with_task("t")
                .with_reason("fatal error"),
        );

        assert_eq!(sub.task_terminal.with_label_values(&["fatal"]).get(), 1.0);
    }

    #[test]
    fn in_flight_does_not_go_negative() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::TaskStopped).with_task("t"));

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn subscriber_overflow_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task("t")
                .with_reason("queue full"),
        );

        assert_eq!(sub.subscriber_overflow.get(), 1.0);
    }

    #[test]
    fn subscriber_panicked_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::SubscriberPanicked)
                .with_task("t")
                .with_reason("boom"),
        );

        assert_eq!(sub.subscriber_panicked.get(), 1.0);
    }

    #[test]
    fn controller_submitted_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::ControllerSubmitted).with_task("t"));

        assert_eq!(sub.controller_submissions.get(), 1.0);
    }

    #[test]
    fn controller_rejected_without_reason_labels_as_unknown() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::ControllerRejected).with_task("t"));

        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["unknown"])
                .get(),
            1.0
        );
    }

    #[test]
    fn controller_rejected_slot_full_reason() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_task("t")
                // taskvisor emits `queue_full: {len}/{max}` (controller/core.rs).
                .with_reason("queue_full: 1/1"),
        );

        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["slot_full"])
                .get(),
            1.0
        );
    }

    #[test]
    fn controller_rejected_slot_busy_reason() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_task("t")
                .with_reason("dropped: slot busy (status=Running)"),
        );

        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["slot_busy"])
                .get(),
            1.0
        );
    }

    #[test]
    fn classify_rejection_reason_recognizes_known_prefixes() {
        assert_eq!(
            // taskvisor emits `queue_full: {len}/{max}` (controller/core.rs).
            classify_rejection_reason(Some("queue_full: 1/1")),
            "slot_full"
        );
        assert_eq!(
            classify_rejection_reason(Some("dropped: slot busy (status=Running)")),
            "slot_busy"
        );
        assert_eq!(
            classify_rejection_reason(Some("add_failed: something")),
            "add_failed"
        );
        assert_eq!(
            classify_rejection_reason(Some("remove_failed: boom")),
            "remove_failed"
        );
        assert_eq!(
            classify_rejection_reason(Some("queue_start_failed: oom")),
            "queue_failed"
        );
        assert_eq!(
            // taskvisor 0.3 emits `recovery_remove_failed: {e}` (controller/core.rs).
            classify_rejection_reason(Some("recovery_remove_failed: net")),
            "recovery_failed"
        );
        assert_eq!(
            classify_rejection_reason(Some("bus_lagged: missed 1 events, recovering slots")),
            "bus_lagged"
        );
        assert_eq!(
            classify_rejection_reason(Some("controller_loop_exited: channel closed")),
            "controller_exited"
        );
    }

    #[test]
    fn classify_rejection_reason_none_is_unknown() {
        assert_eq!(classify_rejection_reason(None), "unknown");
    }

    #[test]
    fn classify_rejection_reason_unrecognized_is_other() {
        assert_eq!(classify_rejection_reason(Some("something weird")), "other");
        assert_eq!(classify_rejection_reason(Some("")), "other");
    }

    #[test]
    fn queue_capacity_defaults_to_2048() {
        let sub = new_subscriber();
        assert_eq!(sub.queue_capacity(), DEFAULT_QUEUE_CAPACITY);
        assert_eq!(sub.queue_capacity(), 2048);
    }

    #[test]
    fn queue_capacity_is_overridable_via_constructor() {
        let registry = Arc::new(Registry::new());
        let sub = PrometheusSubscriber::with_queue_capacity(registry, 4096).unwrap();
        assert_eq!(sub.queue_capacity(), 4096);
    }

    #[test]
    fn shared_registry_with_backend() {
        let registry = Arc::new(Registry::new());

        let backend = crate::PrometheusMetrics::new(registry.clone()).unwrap();
        let sub = PrometheusSubscriber::new(registry.clone()).unwrap();

        backend.record_task_started(solti_runner::RunnerType::Subprocess);
        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        );

        let text = metrics_text(&registry);
        assert!(text.contains("solti_runner_tasks_started_total"));
        assert!(text.contains("solti_sv_tasks_in_flight"));
    }
}
