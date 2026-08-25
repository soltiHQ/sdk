//! # Taskvisor metrics
//!
//! [`PrometheusTaskvisorSubscriber`] implements [`Subscribe`].
//! It translates lifecycle events into Prometheus metrics.
//!
//! Enable it with the `taskvisor` feature.
//!
//! ## Flow
//!
//! ```text
//! Taskvisor runtime
//!       │ best-effort Event
//!       ▼
//! subscriber queue
//!       ▼
//! PrometheusTaskvisorSubscriber ──► Registry
//! ```

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;

use prometheus::{Counter, CounterVec, Gauge, Histogram, Registry};
#[cfg(feature = "taskvisor-controller")]
use taskvisor::RejectionKind;
use taskvisor::{Event, EventKind, Subscribe, TaskId, TaskOutcomeKind};

use crate::register::{MetricGroup, ms_to_secs};

/// Default capacity of the Taskvisor subscriber queue.
pub const DEFAULT_TASKVISOR_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2048).unwrap();

const PROMETHEUS_TASKVISOR_SUBSCRIBER_NAME: &str = "prometheus-taskvisor";
const TASKVISOR_SUBSCRIBER_LISTENER: &str = "subscriber_listener";

/// Prometheus metrics from Taskvisor lifecycle events.
///
/// ## Metrics
///
/// | Metric                                          | Type      | Labels    |
/// |-------------------------------------------------|-----------|-----------|
/// | `solti_taskvisor_attempts_in_flight`            | Gauge     | -         |
/// | `solti_taskvisor_task_restarts_total`           | Counter   | -         |
/// | `solti_taskvisor_task_backoffs_total`           | Counter   | `source`  |
/// | `solti_taskvisor_task_backoff_duration_seconds` | Histogram | -         |
/// | `solti_taskvisor_task_final_outcomes_total`     | Counter   | `outcome` |
/// | `solti_taskvisor_attempt_timeouts_total`        | Counter   | -         |
/// | `solti_taskvisor_subscriber_overflows_total`    | Counter   | -         |
/// | `solti_taskvisor_subscriber_panics_total`       | Counter   | -         |
/// | `solti_taskvisor_runtime_failures_total`        | Counter   | -         |
///
/// The `taskvisor-controller` feature adds:
///
/// | Metric                                              | Type    | Labels   |
/// |-----------------------------------------------------|---------|----------|
/// | `solti_taskvisor_controller_submitted_events_total` | Counter | -        |
/// | `solti_taskvisor_controller_rejections_total`       | Counter | `reason` |
///
/// ## Event Mapping
///
/// ```text
/// AttemptStarting ─────────────► in_flight + 1
///      attempt > 1 ────────────► restarts + 1
///
/// AttemptSucceeded ─┐
/// AttemptCanceled ──┼──────────► remove matching (TaskId, attempt)
/// AttemptFailed ────┤             in_flight - 1 exactly once
/// AttemptTimedOut ──┘             timeouts + 1
///
/// BackoffScheduled ─────────────► backoffs{source}
///      delay_ms present ────────► backoff_duration
///
/// TaskFinished ─────────────────► final_outcomes{outcome}
///
/// SubscriberOverflow ───────────► subscriber_overflows + dropped count
/// SubscriberPanicked ───────────► subscriber_panics
/// RuntimeFailure ───────────────► runtime_failures
/// SubscriberOverflow from this subscriber or subscriber_listener also clears
/// attempt tracking and sets in_flight to NaN.
///
/// taskvisor-controller:
///   ControllerSubmitted ────────► controller_submitted_events
///   ControllerRejected ─────────► controller_rejections{reason}
/// ```
///
/// While tracking is valid, `TaskFinished` repairs the logical in-flight gauge by removing any attempt still tracked for that Task ID.
/// The repair is idempotent when an attempt-level terminal event arrived first.
/// `ForceAborted` and `Panicked` can end an attempt without an attempt-level terminal event.
/// `ForceAborted` does not prove that task code has exited physically.
/// Other Taskvisor events do not change metrics.
///
/// ## Labels
///
/// `source`, `outcome`, and controller `reason` use Taskvisor's typed labels.
/// Missing typed values use `unknown`.
/// Free-form diagnostic text is never used as a label.
///
/// ## Rules
///
/// Taskvisor events are best-effort.
/// A slow subscriber may miss events.
/// Delivered events are correlated by Taskvisor's canonical `(TaskId, attempt)` identity.
/// Events constructed without that runtime metadata use a count-only fallback.
/// At most one attempt can be active for a Task ID.
/// A conflicting active attempt identity proves that the delivered stream is incomplete.
///
/// The first overflow attributed to this subscriber, an overflow from Taskvisor's shared `subscriber_listener`,
/// or a conflicting identity permanently invalidates in-flight tracking because the event stream has
/// no authoritative active-attempt snapshot.
/// The subscriber releases all tracking memory, sets the gauge to `NaN`, and does not retain or
/// apply later attempt identities.
/// Counters, histograms, and final outcomes continue to update from delivered events.
///
/// Taskvisor runtime overflow events always identify their subscriber or internal relay in `task`.
/// An overflow for another subscriber, or a manually constructed event without `task`, still increments
/// the global overflow counter but does not prove that this subscriber missed lifecycle events and does not invalidate it.
/// Taskvisor can coalesce an overflow burst into one event.
/// The overflow counter adds its `dropped` value, or one when no count is available.
/// Controller metrics count delivered controller events.
/// Errors returned before controller intake do not emit `ControllerRejected` and are not included.
///
/// Use `PrometheusCoreStateCollector` for a pull-based task-phase snapshot.
///
/// ## Example
///
/// ```
/// use solti_prometheus::{PrometheusTaskvisorSubscriber, Registry};
/// use taskvisor::{Event, EventKind, Subscribe};
///
/// # fn main() -> Result<(), solti_prometheus::Error> {
/// let registry = Registry::new();
/// let subscriber = PrometheusTaskvisorSubscriber::new(&registry)?;
///
/// subscriber.on_event(
///     &Event::new(EventKind::AttemptStarting).with_attempt(1),
/// );
///
/// assert!(!registry.gather().is_empty());
/// # Ok(()) }
/// ```
pub struct PrometheusTaskvisorSubscriber {
    attempts_in_flight: Gauge,
    in_flight: Mutex<InFlightAttempts>,
    task_restarts: Counter,
    task_backoffs: CounterVec,
    task_backoff_duration: Histogram,
    task_final_outcomes: CounterVec,
    attempt_timeouts: Counter,
    subscriber_overflows: Counter,
    subscriber_panics: Counter,
    runtime_failures: Counter,
    #[cfg(feature = "taskvisor-controller")]
    controller_submitted_events: Counter,
    #[cfg(feature = "taskvisor-controller")]
    controller_rejections: CounterVec,
    queue_capacity: NonZeroUsize,
}

struct InFlightAttempts {
    identified: HashMap<TaskId, u32>,
    anonymous: usize,
    valid: bool,
}

impl Default for InFlightAttempts {
    fn default() -> Self {
        Self {
            identified: HashMap::new(),
            anonymous: 0,
            valid: true,
        }
    }
}

impl InFlightAttempts {
    fn invalidate(&mut self) {
        self.identified = HashMap::new();
        self.anonymous = 0;
        self.valid = false;
    }
}

#[cfg(feature = "taskvisor-controller")]
/// Returns Taskvisor's stable rejection label.
fn rejection_label(kind: Option<RejectionKind>) -> &'static str {
    kind.as_ref()
        .map(RejectionKind::as_label)
        .unwrap_or("unknown")
}

/// Returns Taskvisor's stable final outcome label.
fn terminal_outcome_label(kind: Option<TaskOutcomeKind>) -> &'static str {
    kind.map(TaskOutcomeKind::as_label).unwrap_or("unknown")
}

impl PrometheusTaskvisorSubscriber {
    fn start_attempt(&self, event: &Event) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.valid {
            return;
        }

        let inserted = match (event.id, event.attempt) {
            (Some(id), Some(attempt)) => match in_flight.identified.get(&id).copied() {
                None => {
                    in_flight.identified.insert(id, attempt);
                    true
                }
                Some(active) if active == attempt => false,
                Some(_) => {
                    in_flight.invalidate();
                    self.attempts_in_flight.set(f64::NAN);
                    return;
                }
            },
            _ => {
                let previous = in_flight.anonymous;
                in_flight.anonymous = in_flight.anonymous.saturating_add(1);
                in_flight.anonymous != previous
            }
        };
        if inserted {
            self.attempts_in_flight.inc();
        }
    }

    fn finish_attempt(&self, event: &Event) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.valid {
            return;
        }

        let removed = match (event.id, event.attempt) {
            (Some(id), Some(attempt)) => match in_flight.identified.get(&id).copied() {
                Some(active) if active == attempt => {
                    in_flight.identified.remove(&id);
                    true
                }
                Some(_) => {
                    in_flight.invalidate();
                    self.attempts_in_flight.set(f64::NAN);
                    return;
                }
                None => false,
            },
            _ if in_flight.anonymous > 0 => {
                in_flight.anonymous -= 1;
                true
            }
            _ => false,
        };
        if removed {
            self.attempts_in_flight.dec();
        }
    }

    fn finish_task(&self, event: &Event) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !in_flight.valid {
            return;
        }

        let removed = if let Some(id) = event.id {
            usize::from(in_flight.identified.remove(&id).is_some())
        } else if in_flight.anonymous > 0 {
            in_flight.anonymous -= 1;
            1
        } else {
            0
        };
        if removed > 0 {
            self.attempts_in_flight.sub(removed as f64);
        }
    }

    fn invalidate_in_flight(&self) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if in_flight.valid {
            in_flight.invalidate();
            self.attempts_in_flight.set(f64::NAN);
        }
    }

    fn overflow_invalidates_in_flight(&self, event: &Event) -> bool {
        event
            .task
            .as_deref()
            .is_some_and(|source| source == self.name() || source == TASKVISOR_SUBSCRIBER_LISTENER)
    }

    /// Creates and registers a subscriber with the default queue capacity.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when the metric group cannot be created or registered.
    /// A descriptor conflict returns [`prometheus::Error::AlreadyReg`].
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        Self::with_queue_capacity(registry, DEFAULT_TASKVISOR_QUEUE_CAPACITY)
    }

    /// Creates and registers a subscriber with a specific queue capacity.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when the metric group cannot be created or registered.
    /// A descriptor conflict returns [`prometheus::Error::AlreadyReg`].
    ///
    /// ## Example
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use solti_prometheus::{PrometheusTaskvisorSubscriber, Registry};
    /// use taskvisor::Subscribe;
    ///
    /// # fn main() -> Result<(), solti_prometheus::Error> {
    /// let registry = Registry::new();
    /// let capacity = NonZeroUsize::new(4096).unwrap();
    /// let subscriber = PrometheusTaskvisorSubscriber::with_queue_capacity(&registry, capacity)?;
    ///
    /// assert_eq!(subscriber.queue_capacity().get(), 4096);
    /// # Ok(()) }
    /// ```
    pub fn with_queue_capacity(
        registry: &Registry,
        queue_capacity: NonZeroUsize,
    ) -> Result<Self, prometheus::Error> {
        let mut metrics = MetricGroup::new();

        let attempts_in_flight = metrics.gauge(
            "taskvisor",
            "attempts_in_flight",
            "Active task attempts tracked from delivered Taskvisor events; permanently NaN after detected event loss",
        )?;
        let task_restarts = metrics.counter(
            "taskvisor",
            "task_restarts_total",
            "Total task restarts (attempt > 1)",
        )?;
        let task_backoffs = metrics.counter_vec(
            "taskvisor",
            "task_backoffs_total",
            "Total backoff events",
            &["source"],
        )?;
        let task_backoff_duration = metrics.histogram(
            "taskvisor",
            "task_backoff_duration_seconds",
            "Backoff delay duration in seconds",
            vec![
                0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
                3600.0,
            ],
        )?;
        let task_final_outcomes = metrics.counter_vec(
            "taskvisor",
            "task_final_outcomes_total",
            "Total final task outcomes",
            &["outcome"],
        )?;
        let attempt_timeouts = metrics.counter(
            "taskvisor",
            "attempt_timeouts_total",
            "Total attempt timeout events",
        )?;
        let subscriber_overflows = metrics.counter(
            "taskvisor",
            "subscriber_overflows_total",
            "Total Taskvisor events reported lost by overflow diagnostics",
        )?;
        let subscriber_panics = metrics.counter(
            "taskvisor",
            "subscriber_panics_total",
            "Total subscriber panic events",
        )?;
        let runtime_failures = metrics.counter(
            "taskvisor",
            "runtime_failures_total",
            "Total internal taskvisor runtime failure events",
        )?;

        #[cfg(feature = "taskvisor-controller")]
        let controller_submitted_events = metrics.counter(
            "taskvisor_controller",
            "submitted_events_total",
            "Total ControllerSubmitted events",
        )?;
        #[cfg(feature = "taskvisor-controller")]
        let controller_rejections = metrics.counter_vec(
            "taskvisor_controller",
            "rejections_total",
            "Total ControllerRejected events grouped by typed cause",
            &["reason"],
        )?;
        metrics.register(registry)?;

        Ok(Self {
            attempts_in_flight,
            in_flight: Mutex::new(InFlightAttempts::default()),
            task_restarts,
            task_backoffs,
            task_backoff_duration,
            task_final_outcomes,
            attempt_timeouts,
            subscriber_overflows,
            subscriber_panics,
            runtime_failures,
            #[cfg(feature = "taskvisor-controller")]
            controller_submitted_events,
            #[cfg(feature = "taskvisor-controller")]
            controller_rejections,
            queue_capacity,
        })
    }
}

impl std::fmt::Debug for PrometheusTaskvisorSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusTaskvisorSubscriber").finish()
    }
}

impl Subscribe for PrometheusTaskvisorSubscriber {
    /// Translates a Taskvisor event into Prometheus metric updates.
    fn on_event(&self, event: &Event) {
        match event.kind {
            EventKind::AttemptStarting => {
                self.start_attempt(event);
                if event.attempt.unwrap_or(1) > 1 {
                    self.task_restarts.inc();
                }
            }
            EventKind::AttemptSucceeded | EventKind::AttemptCanceled | EventKind::AttemptFailed => {
                self.finish_attempt(event);
            }
            EventKind::AttemptTimedOut => {
                self.finish_attempt(event);
                self.attempt_timeouts.inc();
            }
            EventKind::SubscriberOverflow => {
                if self.overflow_invalidates_in_flight(event) {
                    self.invalidate_in_flight();
                }
                self.subscriber_overflows
                    .inc_by(event.dropped.unwrap_or(1) as f64);
            }
            EventKind::SubscriberPanicked => {
                self.subscriber_panics.inc();
            }
            EventKind::RuntimeFailure => {
                self.runtime_failures.inc();
            }
            EventKind::BackoffScheduled => {
                let source = event
                    .backoff_source
                    .as_ref()
                    .map(taskvisor::BackoffSource::as_label)
                    .unwrap_or("unknown");
                self.task_backoffs.with_label_values(&[source]).inc();

                if let Some(delay_ms) = event.delay_ms {
                    self.task_backoff_duration
                        .observe(ms_to_secs(delay_ms.into()));
                }
            }
            EventKind::TaskFinished => {
                let label = terminal_outcome_label(event.outcome_kind);
                self.task_final_outcomes.with_label_values(&[label]).inc();
                self.finish_task(event);
            }
            #[cfg(feature = "taskvisor-controller")]
            EventKind::ControllerSubmitted => {
                self.controller_submitted_events.inc();
            }
            #[cfg(feature = "taskvisor-controller")]
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
            | EventKind::GraceExceeded => {}
            #[cfg(feature = "taskvisor-controller")]
            EventKind::ControllerSlotTransition => {}

            _ => {}
        }
    }

    /// Returns `"prometheus-taskvisor"`.
    fn name(&self) -> &'static str {
        PROMETHEUS_TASKVISOR_SUBSCRIBER_NAME
    }

    /// Returns the per-subscriber queue capacity configured at construction.
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

    fn new_subscriber() -> PrometheusTaskvisorSubscriber {
        let registry = Registry::new();
        PrometheusTaskvisorSubscriber::new(&registry).unwrap()
    }

    fn metrics_text(registry: &Registry) -> String {
        let encoder = prometheus::TextEncoder::new();
        let families = registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn attempt_lifecycle_updates_in_flight_and_restarts() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );

        assert_eq!(sub.attempts_in_flight.get(), 1.0);
        assert_eq!(sub.task_restarts.get(), 0.0);
        sub.on_event(&Event::new(EventKind::AttemptSucceeded).with_task("t"));
        assert_eq!(sub.attempts_in_flight.get(), 0.0);

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(2),
        );
        assert_eq!(sub.attempts_in_flight.get(), 1.0);
        assert_eq!(sub.task_restarts.get(), 1.0);
    }

    #[test]
    fn canceled_and_failed_attempts_decrement_in_flight() {
        let sub = new_subscriber();

        for terminal in [EventKind::AttemptCanceled, EventKind::AttemptFailed] {
            sub.on_event(
                &Event::new(EventKind::AttemptStarting)
                    .with_task("t")
                    .with_attempt(1),
            );
            sub.on_event(&Event::new(terminal).with_task("t"));
            assert_eq!(sub.attempts_in_flight.get(), 0.0);
        }
    }

    #[test]
    fn task_finished_uses_typed_outcome_labels_and_unknown_fallback() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::Completed)
                .with_reason("diagnostic text must not select the label"),
        );
        sub.on_event(&Event::new(EventKind::TaskFinished).with_task("t2"));

        assert_eq!(
            sub.task_final_outcomes
                .with_label_values(&["outcome_completed"])
                .get(),
            1.0,
            "completed outcome must retain Taskvisor's label"
        );
        assert_eq!(
            sub.task_final_outcomes
                .with_label_values(&["unknown"])
                .get(),
            1.0
        );
    }

    #[test]
    fn backoff_records_source_and_duration() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::BackoffScheduled)
                .with_task("t")
                .with_delay(Duration::from_secs(5))
                .with_backoff_failure(),
        );
        sub.on_event(
            &Event::new(EventKind::BackoffScheduled)
                .with_task("t")
                .with_delay(Duration::from_secs(10))
                .with_backoff_success(),
        );

        assert_eq!(sub.task_backoffs.with_label_values(&["failure"]).get(), 1.0);
        assert_eq!(sub.task_backoffs.with_label_values(&["success"]).get(), 1.0);
        assert_eq!(sub.task_backoff_duration.get_sample_count(), 2);
        assert_eq!(sub.task_backoff_duration.get_sample_sum(), 15.0);
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

        assert_eq!(sub.attempt_timeouts.get(), 1.0);
        assert_eq!(sub.attempts_in_flight.get(), 0.0);
    }

    #[test]
    fn force_aborted_and_panicked_tasks_repair_in_flight() {
        let sub = new_subscriber();

        for (outcome, label) in [
            (TaskOutcomeKind::ForceAborted, "outcome_force_aborted"),
            (TaskOutcomeKind::Panicked, "outcome_panicked"),
        ] {
            let id = TaskId::for_tests();
            sub.on_event(
                &Event::new(EventKind::AttemptStarting)
                    .with_id(id)
                    .with_task("t")
                    .with_attempt(1),
            );
            sub.on_event(
                &Event::new(EventKind::TaskFinished)
                    .with_id(id)
                    .with_task("t")
                    .with_outcome_kind(outcome),
            );
            assert_eq!(sub.attempts_in_flight.get(), 0.0);
            assert_eq!(
                sub.task_final_outcomes.with_label_values(&[label]).get(),
                1.0
            );
        }
    }

    #[test]
    fn cleanup_panic_sequence_decrements_only_its_identified_attempt() {
        let sub = new_subscriber();
        let panicked = TaskId::for_tests();
        let still_running = TaskId::for_tests();

        for id in [panicked, still_running] {
            sub.on_event(
                &Event::new(EventKind::AttemptStarting)
                    .with_id(id)
                    .with_task("same-name-is-not-the-identity")
                    .with_attempt(1),
            );
        }
        assert_eq!(sub.attempts_in_flight.get(), 2.0);

        sub.on_event(
            &Event::new(EventKind::AttemptFailed)
                .with_id(panicked)
                .with_task("same-name-is-not-the-identity")
                .with_attempt(1),
        );
        assert_eq!(sub.attempts_in_flight.get(), 1.0);

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_id(panicked)
                .with_task("same-name-is-not-the-identity")
                .with_outcome_kind(TaskOutcomeKind::Panicked),
        );
        assert_eq!(
            sub.attempts_in_flight.get(),
            1.0,
            "TaskFinished(Panicked) must be idempotent after AttemptFailed"
        );

        sub.on_event(
            &Event::new(EventKind::AttemptSucceeded)
                .with_id(still_running)
                .with_task("same-name-is-not-the-identity")
                .with_attempt(1),
        );
        assert_eq!(sub.attempts_in_flight.get(), 0.0);
    }

    #[test]
    fn duplicate_identified_events_are_idempotent() {
        let sub = new_subscriber();
        let id = TaskId::for_tests();
        let starting = Event::new(EventKind::AttemptStarting)
            .with_id(id)
            .with_task("t")
            .with_attempt(1);

        sub.on_event(&starting);
        sub.on_event(&starting);
        assert_eq!(sub.attempts_in_flight.get(), 1.0);

        let finished = Event::new(EventKind::AttemptSucceeded)
            .with_id(id)
            .with_task("t")
            .with_attempt(1);
        sub.on_event(&finished);
        sub.on_event(&finished);
        assert_eq!(sub.attempts_in_flight.get(), 0.0);
    }

    #[test]
    fn in_flight_does_not_go_negative() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::AttemptSucceeded).with_task("t"));

        assert_eq!(sub.attempts_in_flight.get(), 0.0);
    }

    #[test]
    fn internal_events_increment_separate_counters() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task("t")
                .with_reason("queue full"),
        );

        sub.on_event(
            &Event::new(EventKind::SubscriberPanicked)
                .with_task("t")
                .with_reason("boom"),
        );
        sub.on_event(
            &Event::new(EventKind::RuntimeFailure)
                .with_task("registry")
                .with_reason("listener join failed"),
        );

        assert_eq!(sub.subscriber_overflows.get(), 1.0);
        assert_eq!(sub.subscriber_panics.get(), 1.0);
        assert_eq!(sub.runtime_failures.get(), 1.0);
    }

    #[test]
    fn subscriber_overflow_counts_every_reported_lost_event() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task("another-subscriber")
                .with_dropped(2),
        );
        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task(sub.name())
                .with_dropped(3),
        );
        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task(TASKVISOR_SUBSCRIBER_LISTENER)
                .with_dropped(5),
        );
        sub.on_event(&Event::new(EventKind::SubscriberOverflow).with_dropped(7));

        assert_eq!(sub.subscriber_overflows.get(), 17.0);
    }

    #[test]
    fn other_or_missing_overflow_source_preserves_in_flight_tracking() {
        let sub = new_subscriber();
        let id = TaskId::for_tests();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_id(id)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task("another-subscriber")
                .with_dropped(2),
        );
        sub.on_event(&Event::new(EventKind::SubscriberOverflow).with_dropped(3));

        assert_eq!(sub.attempts_in_flight.get(), 1.0);
        assert_eq!(sub.subscriber_overflows.get(), 5.0);
        {
            let in_flight = sub.in_flight.lock().unwrap();
            assert!(in_flight.valid);
            assert_eq!(in_flight.identified.get(&id), Some(&1));
        }

        sub.on_event(
            &Event::new(EventKind::AttemptSucceeded)
                .with_id(id)
                .with_task("t")
                .with_attempt(1),
        );
        assert_eq!(sub.attempts_in_flight.get(), 0.0);
    }

    #[test]
    fn subscriber_listener_overflow_invalidates_in_flight_tracking() {
        let sub = new_subscriber();
        let id = TaskId::for_tests();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_id(id)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task(TASKVISOR_SUBSCRIBER_LISTENER)
                .with_dropped(1),
        );

        assert!(sub.attempts_in_flight.get().is_nan());
        let in_flight = sub.in_flight.lock().unwrap();
        assert!(!in_flight.valid);
        assert_eq!(in_flight.identified.capacity(), 0);
        assert_eq!(in_flight.anonymous, 0);
    }

    #[test]
    fn own_overflow_releases_tracking_and_permanently_sets_in_flight_to_nan() {
        let registry = Registry::new();
        let sub = PrometheusTaskvisorSubscriber::new(&registry).unwrap();

        for index in 0..4_096 {
            sub.on_event(
                &Event::new(EventKind::AttemptStarting)
                    .with_id(TaskId::for_tests())
                    .with_task(format!("t-{index}"))
                    .with_attempt(1),
            );
        }
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("anonymous")
                .with_attempt(1),
        );
        {
            let in_flight = sub.in_flight.lock().unwrap();
            assert!(in_flight.valid);
            assert_eq!(in_flight.identified.len(), 4_096);
            assert!(in_flight.identified.capacity() >= 4_096);
            assert_eq!(in_flight.anonymous, 1);
        }

        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task(sub.name())
                .with_dropped(7),
        );
        assert!(sub.attempts_in_flight.get().is_nan());
        let text = metrics_text(&registry);
        assert!(text.contains(
            "# HELP solti_taskvisor_attempts_in_flight Active task attempts tracked from delivered Taskvisor events; permanently NaN after detected event loss"
        ));
        assert!(text.contains("solti_taskvisor_attempts_in_flight NaN"));
        {
            let in_flight = sub.in_flight.lock().unwrap();
            assert!(!in_flight.valid);
            assert_eq!(in_flight.identified.len(), 0);
            assert_eq!(in_flight.identified.capacity(), 0);
            assert_eq!(in_flight.anonymous, 0);
        }

        let ignored = TaskId::for_tests();
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_id(ignored)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("anonymous-after-overflow")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::AttemptTimedOut)
                .with_id(ignored)
                .with_task("t")
                .with_attempt(1),
        );
        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_id(ignored)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::Completed),
        );
        assert!(sub.attempts_in_flight.get().is_nan());
        assert_eq!(sub.attempt_timeouts.get(), 1.0);
        assert_eq!(
            sub.task_final_outcomes
                .with_label_values(&["outcome_completed"])
                .get(),
            1.0
        );
        let in_flight = sub.in_flight.lock().unwrap();
        assert!(!in_flight.valid);
        assert_eq!(in_flight.identified.capacity(), 0);
        assert_eq!(in_flight.anonymous, 0);
    }

    #[test]
    fn conflicting_active_attempt_identity_invalidates_tracking() {
        for conflict in [EventKind::AttemptStarting, EventKind::AttemptFailed] {
            let sub = new_subscriber();
            let id = TaskId::for_tests();

            sub.on_event(
                &Event::new(EventKind::AttemptStarting)
                    .with_id(id)
                    .with_task("t")
                    .with_attempt(1),
            );
            sub.on_event(
                &Event::new(conflict)
                    .with_id(id)
                    .with_task("t")
                    .with_attempt(2),
            );

            assert!(sub.attempts_in_flight.get().is_nan());
            let in_flight = sub.in_flight.lock().unwrap();
            assert!(!in_flight.valid);
            assert_eq!(in_flight.identified.capacity(), 0);
            assert_eq!(in_flight.anonymous, 0);
        }
    }

    #[cfg(feature = "taskvisor-controller")]
    #[test]
    fn controller_events_update_their_metrics() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::ControllerSubmitted).with_task("t"));
        sub.on_event(&Event::new(EventKind::ControllerRejected).with_task("t"));
        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_task("t")
                .with_rejection_kind(RejectionKind::QueueFull)
                .with_reason("queue_full: 1/1"),
        );
        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_task("t")
                .with_rejection_kind(RejectionKind::ResourceLimit)
                .with_reason("controller pending limit reached"),
        );

        assert_eq!(sub.controller_submitted_events.get(), 1.0);
        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["unknown"])
                .get(),
            1.0
        );
        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["queue_full"])
                .get(),
            1.0
        );
        assert_eq!(
            sub.controller_rejections
                .with_label_values(&["resource_limit"])
                .get(),
            1.0
        );
    }

    #[test]
    fn queue_capacity_supports_default_and_override() {
        let sub = new_subscriber();
        assert_eq!(sub.queue_capacity(), DEFAULT_TASKVISOR_QUEUE_CAPACITY);

        let registry = Registry::new();
        let capacity = NonZeroUsize::new(4096).unwrap();
        let sub = PrometheusTaskvisorSubscriber::with_queue_capacity(&registry, capacity).unwrap();
        assert_eq!(sub.queue_capacity().get(), 4096);
    }

    #[cfg(feature = "runner")]
    #[test]
    fn shared_registry_with_backend() {
        let registry = Registry::new();

        let backend = crate::PrometheusRunnerMetrics::new(&registry).unwrap();
        let sub = PrometheusTaskvisorSubscriber::new(&registry).unwrap();

        backend.record_runner_error(
            solti_runner::RunnerType::Subprocess,
            solti_runner::RunnerErrorKind::SpawnFailed,
        );
        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );

        let text = metrics_text(&registry);
        assert!(text.contains("solti_runner_errors_total"));
        assert!(text.contains("solti_taskvisor_attempts_in_flight"));
    }
}
