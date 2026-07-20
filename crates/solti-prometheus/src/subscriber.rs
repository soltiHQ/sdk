//! # Supervision Prometheus metrics.
//!
//! [`PrometheusSubscriber`] implements [`Subscribe`] and translates [`taskvisor`] events into Prometheus counters, gauges, and histograms.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::{num::NonZeroUsize, sync::Arc};

use prometheus::{Counter, CounterVec, Gauge, Histogram, Registry};
use taskvisor::{BackoffSource, Event, EventKind, RejectionKind, Subscribe, TaskOutcomeKind};

use crate::register::{Sub, ms_to_secs};

/// Default subscriber queue capacity.
///
/// This is larger than taskvisor's basic examples because metrics subscribers are often used under bursty workloads.
pub const DEFAULT_QUEUE_CAPACITY: usize = 2048;

/// Prometheus subscriber for supervision metrics.
///
/// Implements [`Subscribe`] and captures metrics from the [`taskvisor`] event stream.
/// Share the same [`Registry`] with [`crate::PrometheusMetrics`] for one `/metrics` endpoint.
///
/// ## Event Mapping
///
/// ```text
/// AttemptStarting       -> tasks_in_flight.inc()
///                          task_restarts.inc() if attempt > 1
/// AttemptSucceeded      -> tasks_in_flight.dec()
/// AttemptCanceled       -> tasks_in_flight.dec()
/// AttemptFailed         -> tasks_in_flight.dec()
/// AttemptTimedOut       -> tasks_in_flight.dec()
///                          task_timeouts.inc()
/// BackoffScheduled      -> task_backoff_count{source}.inc()
///                          task_backoff_duration.observe(delay)
/// TaskFinished          -> task_terminal{outcome}.inc()
///                          tasks_in_flight.dec() for force-abort/panic fallback
/// SubscriberOverflow    -> subscriber_overflow.inc()
/// SubscriberPanicked    -> subscriber_panicked.inc()
/// RuntimeFailure        -> runtime_failures.inc()
/// ControllerSubmitted   -> controller_submissions.inc()
/// ControllerRejected    -> controller_rejections{reason}.inc()
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
/// | `solti_sv_task_terminal_total`           | Counter   | `outcome`| Final task outcomes          |
/// | `solti_sv_task_timeouts_total`           | Counter   | -        | Timeout events               |
/// | `solti_sv_subscriber_overflow_total`     | Counter   | -        | Queue overflow (lost events) |
/// | `solti_sv_subscriber_panicked_total`     | Counter   | -        | Subscriber panics            |
/// | `solti_sv_runtime_failures_total`        | Counter   | -        | Internal runtime failures    |
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
/// | `outcome` | `completed`, `exhausted`, `fatal`, `canceled`, `force_aborted`, `panicked`, `rejected`, `other`, `unknown` | [`TaskOutcomeKind`] on `TaskFinished` |
/// | `reason` (rejection)  | `slot_full`, `slot_busy`, `superseded`, `removed`, `shutting_down`, `admission_failed`, `already_exists`, `batch_rejected`, `other`, `unknown` | Mapped from [`RejectionKind`] |
///
/// ## Notes
///
/// - `tasks_in_flight` is best-effort. It is derived from taskvisor's lossy broadcast bus
///   (inc on `AttemptStarting`, dec on the per-attempt terminal, with a
///   `TaskFinished` fallback for force-abort or internal runner panic).
///   A dropped event under sustained bus lag can make it drift.
///   It is guarded against going negative.
///   For an authoritative count that is recomputed from `TaskState` on every scrape, use the pull-based `PrometheusStateCollector`
///   (`state` feature): `solti_sv_tasks_by_phase{phase="running"}`.
/// - [`queue_capacity`](Subscribe::queue_capacity) defaults to [`DEFAULT_QUEUE_CAPACITY`].
/// - Backoff duration is converted from milliseconds to seconds before observation.
/// - The terminal `rejected` label is defensive. Current Taskvisor admission
///   rejections use `ControllerRejected` and `solti_ctrl_rejections_total`;
///   they do not emit `TaskFinished`.
///
/// ## Also
///
/// - [`PrometheusMetrics`](crate::PrometheusMetrics): runner-level metrics, complementary to this subscriber.
/// - [`Event`](taskvisor::Event) and [`EventKind`](taskvisor::EventKind): event structure and classification.
pub struct PrometheusSubscriber {
    tasks_in_flight: Gauge,
    task_restarts: Counter,
    task_backoff_count: CounterVec,
    task_backoff_duration: Histogram,
    task_terminal: CounterVec,
    task_timeouts: Counter,
    subscriber_overflow: Counter,
    subscriber_panicked: Counter,
    runtime_failures: Counter,
    controller_submissions: Counter,
    controller_rejections: CounterVec,
    queue_capacity: NonZeroUsize,
}

/// Map a typed taskvisor rejection to the SDK's bounded Prometheus label set.
fn rejection_label(kind: Option<RejectionKind>) -> &'static str {
    match kind {
        None => "unknown",
        Some(RejectionKind::QueueFull) => "slot_full",
        Some(RejectionKind::SlotBusy) => "slot_busy",
        Some(RejectionKind::SupersededByReplace) => "superseded",
        Some(RejectionKind::RemovedFromQueue) => "removed",
        Some(RejectionKind::ControllerShuttingDown) => "shutting_down",
        Some(RejectionKind::AdmissionFailed) => "admission_failed",
        Some(RejectionKind::AlreadyExists) => "already_exists",
        Some(RejectionKind::BatchRejected) => "batch_rejected",
        Some(_) => "other",
    }
}

/// Map Taskvisor's typed final outcome to the SDK's bounded metric labels.
fn terminal_outcome_label(kind: Option<TaskOutcomeKind>) -> &'static str {
    match kind {
        None => "unknown",
        Some(TaskOutcomeKind::Completed) => "completed",
        Some(TaskOutcomeKind::Failed) => "exhausted",
        Some(TaskOutcomeKind::Fatal) => "fatal",
        Some(TaskOutcomeKind::Canceled) => "canceled",
        Some(TaskOutcomeKind::ForceAborted) => "force_aborted",
        Some(TaskOutcomeKind::Panicked) => "panicked",
        Some(TaskOutcomeKind::Rejected) => "rejected",
        Some(_) => "other",
    }
}

impl PrometheusSubscriber {
    fn decrement_in_flight(&self) {
        if self.tasks_in_flight.get() > 0.0 {
            self.tasks_in_flight.dec();
        }
    }

    /// Create a new subscriber with the default event-bus queue capacity ([`DEFAULT_QUEUE_CAPACITY`]).
    ///
    /// ## Errors
    ///
    /// - [`prometheus::Error::AlreadyReg`]: one of the `solti_sv_*` / `solti_ctrl_*` metrics is
    ///   already registered in `registry` (e.g. another subscriber was built against the same registry).
    ///
    /// ## Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use solti_prometheus::{PrometheusSubscriber, Registry};
    /// use taskvisor::{Event, EventKind, Subscribe};
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let registry = Arc::new(Registry::new());
    /// let subscriber = PrometheusSubscriber::new(registry.clone())?;
    ///
    /// subscriber.on_event(&Event::new(EventKind::AttemptStarting).with_attempt(1));
    ///
    /// assert!(!registry.gather().is_empty());
    /// # Ok(()) }
    /// ```
    pub fn new(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        Self::with_queue_capacity(registry, DEFAULT_QUEUE_CAPACITY)
    }

    /// Create a new subscriber with a specific event-bus queue capacity.
    /// A capacity of zero is normalized to one for backward compatibility.
    ///
    /// ## Errors
    ///
    /// - [`prometheus::Error::AlreadyReg`]: one of the `solti_sv_*` / `solti_ctrl_*` metrics is
    ///   already registered in `registry` (e.g. another subscriber was built against the same registry).
    ///
    /// ## Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use solti_prometheus::{PrometheusSubscriber, Registry};
    /// use taskvisor::Subscribe;
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let registry = Arc::new(Registry::new());
    /// let subscriber = PrometheusSubscriber::with_queue_capacity(registry, 4096)?;
    ///
    /// assert_eq!(subscriber.queue_capacity().get(), 4096);
    /// # Ok(()) }
    /// ```
    pub fn with_queue_capacity(
        registry: Arc<Registry>,
        queue_capacity: usize,
    ) -> Result<Self, prometheus::Error> {
        // Preserve the SDK's zero-to-one normalization while satisfying
        // taskvisor's typed non-zero subscriber contract.
        let queue_capacity = NonZeroUsize::new(queue_capacity).unwrap_or(NonZeroUsize::MIN);
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
            "Total final task outcomes",
            &["outcome"],
        )?;
        let task_timeouts = sv.counter("task_timeouts_total", "Total task timeout events")?;
        let subscriber_overflow = sv.counter(
            "subscriber_overflow_total",
            "Total subscriber queue overflow events (events lost)",
        )?;
        let subscriber_panicked =
            sv.counter("subscriber_panicked_total", "Total subscriber panic events")?;
        let runtime_failures = sv.counter(
            "runtime_failures_total",
            "Total internal taskvisor runtime failure events",
        )?;

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
            task_timeouts,
            subscriber_overflow,
            subscriber_panicked,
            runtime_failures,
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
            EventKind::AttemptStarting => {
                self.tasks_in_flight.inc();
                if event.attempt.unwrap_or(1) > 1 {
                    self.task_restarts.inc();
                }
            }
            EventKind::AttemptSucceeded | EventKind::AttemptCanceled | EventKind::AttemptFailed => {
                self.decrement_in_flight();
            }
            EventKind::AttemptTimedOut => {
                self.decrement_in_flight();
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
            EventKind::RuntimeFailure => {
                tracing::error!(
                    component = event.task.as_deref().unwrap_or("unknown"),
                    reason = event.reason.as_deref().unwrap_or("unknown"),
                    "taskvisor runtime component failed"
                );
                self.runtime_failures.inc();
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
            EventKind::TaskFinished => {
                let label = terminal_outcome_label(event.outcome_kind);
                self.task_terminal.with_label_values(&[label]).inc();

                // Force-abort and an internal runner panic can end a running
                // attempt without an attempt-level terminal event.
                if matches!(
                    event.outcome_kind,
                    Some(TaskOutcomeKind::ForceAborted | TaskOutcomeKind::Panicked)
                ) {
                    self.decrement_in_flight();
                }
            }
            EventKind::ControllerSubmitted => {
                self.controller_submissions.inc();
            }
            EventKind::ControllerRejected => {
                let reason = rejection_label(event.rejection_kind);
                self.controller_rejections
                    .with_label_values(&[reason])
                    .inc();
            }
            EventKind::TaskRemoved => {}
            EventKind::TaskAdded
            | EventKind::TaskAddFailed
            | EventKind::TaskAddRequested
            | EventKind::TaskRemoveRequested
            | EventKind::ShutdownRequested
            | EventKind::AllStoppedWithinGrace
            | EventKind::GraceExceeded
            | EventKind::ControllerSlotTransition => {}

            _ => {}
        }
    }

    /// Returns `"prometheus"`.
    fn name(&self) -> &'static str {
        "prometheus"
    }

    /// Returns the per-subscriber queue capacity configured via [`PrometheusSubscriber::new`] or [`PrometheusSubscriber::with_queue_capacity`].
    fn queue_capacity(&self) -> NonZeroUsize {
        self.queue_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Encoder;
    #[cfg(feature = "runner")]
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
    fn attempt_starting_increments_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );

        assert_eq!(sub.tasks_in_flight.get(), 1.0);
    }

    #[test]
    fn attempt_succeeded_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(&Event::new(EventKind::AttemptSucceeded).with_task("t"));

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn attempt_canceled_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(&Event::new(EventKind::AttemptCanceled).with_task("t"));

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn force_aborted_task_finished_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::ForceAborted),
        );

        assert_eq!(
            sub.tasks_in_flight.get(),
            0.0,
            "force-aborted tasks must not leak the in-flight gauge"
        );
    }

    #[test]
    fn task_finished_uses_typed_outcome_labels() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::Completed)
                .with_reason("diagnostic text must not select the label"),
        );
        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t2")
                .with_outcome_kind(TaskOutcomeKind::Failed)
                .with_reason("different diagnostic text"),
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
    fn rejection_labels_preserve_existing_public_categories() {
        assert_eq!(
            rejection_label(Some(RejectionKind::SupersededByReplace)),
            "superseded"
        );
        assert_eq!(
            rejection_label(Some(RejectionKind::RemovedFromQueue)),
            "removed"
        );
        assert_eq!(
            rejection_label(Some(RejectionKind::ControllerShuttingDown)),
            "shutting_down"
        );
    }

    #[test]
    fn attempt_failed_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::AttemptFailed)
                .with_task("t")
                .with_reason("boom"),
        );

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn first_attempt_is_not_a_restart() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );

        assert_eq!(sub.task_restarts.get(), 0.0);
    }

    #[test]
    fn second_attempt_is_a_restart() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
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
    fn attempt_timed_out_is_terminal_and_increments_timeout_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::AttemptTimedOut)
                .with_task("t")
                .with_timeout(Duration::from_secs(30)),
        );

        assert_eq!(sub.task_timeouts.get(), 1.0);
        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[test]
    fn terminal_outcome_labels_cover_current_typed_kinds() {
        for (kind, expected) in [
            (Some(TaskOutcomeKind::Completed), "completed"),
            (Some(TaskOutcomeKind::Failed), "exhausted"),
            (Some(TaskOutcomeKind::Fatal), "fatal"),
            (Some(TaskOutcomeKind::Canceled), "canceled"),
            (Some(TaskOutcomeKind::ForceAborted), "force_aborted"),
            (Some(TaskOutcomeKind::Panicked), "panicked"),
            (Some(TaskOutcomeKind::Rejected), "rejected"),
            (None, "unknown"),
        ] {
            assert_eq!(terminal_outcome_label(kind), expected);
        }
    }

    #[test]
    fn task_finished_panicked_counts_terminal_and_repairs_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::Panicked),
        );

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
        assert_eq!(
            sub.task_terminal.with_label_values(&["panicked"]).get(),
            1.0
        );
    }

    #[test]
    fn terminal_metrics_do_not_reconstruct_missing_attempt_counts() {
        let registry = Arc::new(Registry::new());
        let sub = PrometheusSubscriber::new(Arc::clone(&registry)).unwrap();

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::Completed),
        );

        let text = metrics_text(&registry);
        assert!(text.contains("solti_sv_task_terminal_total"));
        assert!(!text.contains("solti_sv_attempts_to_finalize"));
    }

    #[test]
    fn in_flight_does_not_go_negative() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::AttemptSucceeded).with_task("t"));

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
        assert_eq!(sub.runtime_failures.get(), 0.0);
    }

    #[test]
    fn runtime_failure_increments_separate_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::RuntimeFailure)
                .with_task("registry")
                .with_reason("listener join failed"),
        );

        assert_eq!(sub.runtime_failures.get(), 1.0);
        assert_eq!(sub.subscriber_panicked.get(), 0.0);
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
                .with_rejection_kind(RejectionKind::QueueFull)
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
                .with_rejection_kind(RejectionKind::SlotBusy)
                .with_reason("slot is busy; diagnostic text is not classification"),
        );

        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["slot_busy"])
                .get(),
            1.0
        );
    }

    #[test]
    fn rejection_label_covers_current_typed_kinds() {
        for (kind, expected) in [
            (RejectionKind::QueueFull, "slot_full"),
            (RejectionKind::SlotBusy, "slot_busy"),
            (RejectionKind::AdmissionFailed, "admission_failed"),
            (RejectionKind::AlreadyExists, "already_exists"),
            (RejectionKind::BatchRejected, "batch_rejected"),
        ] {
            assert_eq!(rejection_label(Some(kind)), expected);
        }
    }

    #[test]
    fn missing_rejection_kind_is_unknown() {
        assert_eq!(rejection_label(None), "unknown");
    }

    #[test]
    fn queue_capacity_defaults_to_2048() {
        let sub = new_subscriber();
        assert_eq!(sub.queue_capacity().get(), DEFAULT_QUEUE_CAPACITY);
        assert_eq!(sub.queue_capacity().get(), 2048);
    }

    #[test]
    fn queue_capacity_is_overridable_via_constructor() {
        let registry = Arc::new(Registry::new());
        let sub = PrometheusSubscriber::with_queue_capacity(registry, 4096).unwrap();
        assert_eq!(sub.queue_capacity().get(), 4096);
    }

    #[test]
    fn zero_queue_capacity_preserves_the_previous_minimum_of_one() {
        let registry = Arc::new(Registry::new());
        let sub = PrometheusSubscriber::with_queue_capacity(registry, 0).unwrap();
        assert_eq!(sub.queue_capacity().get(), 1);
    }

    #[cfg(feature = "runner")]
    #[test]
    fn shared_registry_with_backend() {
        let registry = Arc::new(Registry::new());

        let backend = crate::PrometheusMetrics::new(registry.clone()).unwrap();
        let sub = PrometheusSubscriber::new(registry.clone()).unwrap();

        backend.record_task_started(solti_runner::RunnerType::Subprocess);
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );

        let text = metrics_text(&registry);
        assert!(text.contains("solti_runner_tasks_started_total"));
        assert!(text.contains("solti_sv_tasks_in_flight"));
    }
}
