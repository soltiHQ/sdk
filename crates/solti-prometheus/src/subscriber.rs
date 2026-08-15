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
//!       │
//!       ▼
//! PrometheusTaskvisorSubscriber ──► Registry
//! ```

use std::num::NonZeroUsize;

use prometheus::{Counter, CounterVec, Gauge, Histogram, Registry};
#[cfg(feature = "taskvisor-controller")]
use taskvisor::RejectionKind;
use taskvisor::{Event, EventKind, Subscribe, TaskOutcomeKind};

use crate::register::{MetricGroup, ms_to_secs};

/// Default capacity of the Taskvisor subscriber queue.
pub const DEFAULT_TASKVISOR_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2048).unwrap();

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
/// AttemptCanceled ──┼──────────► in_flight - 1
/// AttemptFailed ────┤
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
///
/// taskvisor-controller:
///   ControllerSubmitted ────────► controller_submitted_events
///   ControllerRejected ─────────► controller_rejections{reason}
/// ```
///
/// `TaskFinished` with `ForceAborted` or `Panicked` also repairs the logical
/// in-flight gauge. Those outcomes can end an attempt without an attempt-level
/// terminal event. `ForceAborted` does not prove that task code has exited physically.
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
/// The in-flight gauge can therefore drift.
/// The decrement path checks the current value before changing the gauge.
/// Taskvisor 0.8 can coalesce an overflow burst into one event.
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
    fn decrement_in_flight(&self) {
        if self.attempts_in_flight.get() > 0.0 {
            self.attempts_in_flight.dec();
        }
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
            "Number of task attempts active in delivered Taskvisor lifecycle events",
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
                self.attempts_in_flight.inc();
                if event.attempt.unwrap_or(1) > 1 {
                    self.task_restarts.inc();
                }
            }
            EventKind::AttemptSucceeded | EventKind::AttemptCanceled | EventKind::AttemptFailed => {
                self.decrement_in_flight();
            }
            EventKind::AttemptTimedOut => {
                self.decrement_in_flight();
                self.attempt_timeouts.inc();
            }
            EventKind::SubscriberOverflow => {
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

                if matches!(
                    event.outcome_kind,
                    Some(TaskOutcomeKind::ForceAborted | TaskOutcomeKind::Panicked)
                ) {
                    self.decrement_in_flight();
                }
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
        "prometheus-taskvisor"
    }

    /// Returns the per-subscriber queue capacity configured at construction.
    fn queue_capacity(&self) -> NonZeroUsize {
        self.queue_capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "runner")]
    use prometheus::Encoder;
    #[cfg(feature = "runner")]
    use solti_runner::MetricsBackend;
    use std::time::Duration;

    fn new_subscriber() -> PrometheusTaskvisorSubscriber {
        let registry = Registry::new();
        PrometheusTaskvisorSubscriber::new(&registry).unwrap()
    }

    #[cfg(feature = "runner")]
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
            sub.on_event(
                &Event::new(EventKind::AttemptStarting)
                    .with_task("t")
                    .with_attempt(1),
            );
            sub.on_event(
                &Event::new(EventKind::TaskFinished)
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
                .with_task("subscriber-listener")
                .with_dropped(7),
        );
        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task("closed-subscriber")
                .with_reason("closed"),
        );

        assert_eq!(sub.subscriber_overflows.get(), 8.0);
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
