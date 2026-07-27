//! # Supervision Prometheus metrics.
//!
//! [`PrometheusTaskvisorSubscriber`] implements [`Subscribe`] and translates [`taskvisor`] events into Prometheus counters, gauges, and histograms.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::num::NonZeroUsize;

use prometheus::{Counter, CounterVec, Gauge, Histogram, Registry};
#[cfg(any(feature = "taskvisor-controller", test))]
use taskvisor::RejectionKind;
use taskvisor::{Event, EventKind, Subscribe, TaskOutcomeKind};

use crate::register::{MetricGroup, ms_to_secs};

/// Default subscriber queue capacity.
///
/// This is larger than taskvisor's basic examples because metrics subscribers are often used under bursty workloads.
pub const DEFAULT_TASKVISOR_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2048).unwrap();

/// Prometheus subscriber for supervision metrics.
///
/// It implements [`Subscribe`] and records Taskvisor lifecycle events.
/// Events are best-effort and may be dropped by Taskvisor.
///
/// # Metrics
///
/// | Metric                                             | Labels    |
/// |----------------------------------------------------|-----------|
/// | `solti_taskvisor_attempts_in_flight`               | -         |
/// | `solti_taskvisor_task_restarts_total`              | -         |
/// | `solti_taskvisor_task_backoffs_total`              | `source`  |
/// | `solti_taskvisor_task_backoff_duration_seconds`    | -         |
/// | `solti_taskvisor_task_final_outcomes_total`        | `outcome` |
/// | `solti_taskvisor_attempt_timeouts_total`           | -         |
/// | `solti_taskvisor_subscriber_overflows_total`       | -         |
/// | `solti_taskvisor_subscriber_panics_total`          | -         |
/// | `solti_taskvisor_runtime_failures_total`           | -         |
///
/// Feature `taskvisor-controller` adds:
///
/// | Metric                                                   | Labels   |
/// |----------------------------------------------------------|----------|
/// | `solti_taskvisor_controller_submitted_events_total`      | -        |
/// | `solti_taskvisor_controller_rejections_total`            | `reason` |
///
/// # Labels
///
/// Labels come from Taskvisor's typed `as_label()` methods.
/// Diagnostic text is never used as a label.
///
/// # Current state
///
/// `solti_taskvisor_attempts_in_flight` follows the lossy event stream and may
/// drift after a dropped event. `PrometheusCoreStateCollector` provides the
/// pull-based resource phase snapshot under feature `state`.
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

#[cfg(any(feature = "taskvisor-controller", test))]
/// Returns Taskvisor's stable rejection label.
fn rejection_label(kind: Option<RejectionKind>) -> &'static str {
    kind.as_ref()
        .map(RejectionKind::as_label)
        .unwrap_or("unknown")
}

/// Return Taskvisor's stable final outcome label.
fn terminal_outcome_label(kind: Option<TaskOutcomeKind>) -> &'static str {
    kind.map(TaskOutcomeKind::as_label).unwrap_or("unknown")
}

impl PrometheusTaskvisorSubscriber {
    fn decrement_in_flight(&self) {
        if self.attempts_in_flight.get() > 0.0 {
            self.attempts_in_flight.dec();
        }
    }

    /// Create a subscriber with [`DEFAULT_TASKVISOR_QUEUE_CAPACITY`].
    ///
    /// # Errors
    ///
    /// Returns [`prometheus::Error::AlreadyReg`] when this metric group already
    /// exists in `registry`.
    ///
    /// # Example
    ///
    /// ```
    /// use solti_prometheus::{PrometheusTaskvisorSubscriber, Registry};
    /// use taskvisor::{Event, EventKind, Subscribe};
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let registry = Registry::new();
    /// let subscriber = PrometheusTaskvisorSubscriber::new(&registry)?;
    ///
    /// subscriber.on_event(&Event::new(EventKind::AttemptStarting).with_attempt(1));
    ///
    /// assert!(!registry.gather().is_empty());
    /// # Ok(()) }
    /// ```
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        Self::with_queue_capacity(registry, DEFAULT_TASKVISOR_QUEUE_CAPACITY)
    }

    /// Create a subscriber with a specific event-bus queue capacity.
    ///
    /// # Errors
    ///
    /// Returns [`prometheus::Error::AlreadyReg`] when this metric group already
    /// exists in `registry`.
    ///
    /// # Example
    ///
    /// ```
    /// use std::num::NonZeroUsize;
    /// use solti_prometheus::{PrometheusTaskvisorSubscriber, Registry};
    /// use taskvisor::Subscribe;
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
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
            "Number of task attempts currently executing",
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
            "Total subscriber queue overflow events (events lost)",
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
            "Total controller rejections grouped by cause",
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
    /// Translates a [`taskvisor`] event into prometheus metric updates.
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
                self.subscriber_overflows.inc();
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

                // Force-abort and an internal runner panic can end a running
                // attempt without an attempt-level terminal event.
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
    fn attempt_starting_increments_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::AttemptStarting)
                .with_task("t")
                .with_attempt(1),
        );

        assert_eq!(sub.attempts_in_flight.get(), 1.0);
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

        assert_eq!(sub.attempts_in_flight.get(), 0.0);
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

        assert_eq!(sub.attempts_in_flight.get(), 0.0);
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
            sub.attempts_in_flight.get(),
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
            sub.task_final_outcomes
                .with_label_values(&["outcome_completed"])
                .get(),
            1.0,
            "completed outcome must retain Taskvisor's label"
        );
        assert_eq!(
            sub.task_final_outcomes
                .with_label_values(&["outcome_failed"])
                .get(),
            1.0
        );
    }

    #[test]
    fn rejection_labels_preserve_taskvisor_categories() {
        assert_eq!(
            rejection_label(Some(RejectionKind::SupersededByReplace)),
            "superseded_by_replace"
        );
        assert_eq!(
            rejection_label(Some(RejectionKind::RemovedFromQueue)),
            "removed_from_queue"
        );
        assert_eq!(
            rejection_label(Some(RejectionKind::ControllerShuttingDown)),
            "controller_shutting_down"
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

        assert_eq!(sub.attempts_in_flight.get(), 0.0);
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

        assert_eq!(sub.task_backoffs.with_label_values(&["failure"]).get(), 1.0);
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

        assert_eq!(sub.task_backoffs.with_label_values(&["success"]).get(), 1.0);
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
    fn terminal_outcome_labels_cover_current_typed_kinds() {
        for (kind, expected) in [
            (Some(TaskOutcomeKind::Completed), "outcome_completed"),
            (Some(TaskOutcomeKind::Failed), "outcome_failed"),
            (Some(TaskOutcomeKind::Fatal), "outcome_fatal"),
            (Some(TaskOutcomeKind::Canceled), "outcome_canceled"),
            (Some(TaskOutcomeKind::ForceAborted), "outcome_force_aborted"),
            (Some(TaskOutcomeKind::Panicked), "outcome_panicked"),
            (Some(TaskOutcomeKind::Rejected), "outcome_rejected"),
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

        assert_eq!(sub.attempts_in_flight.get(), 0.0);
        assert_eq!(
            sub.task_final_outcomes
                .with_label_values(&["outcome_panicked"])
                .get(),
            1.0
        );
    }

    #[test]
    fn terminal_metrics_do_not_reconstruct_missing_attempt_counts() {
        let registry = Registry::new();
        let sub = PrometheusTaskvisorSubscriber::new(&registry).unwrap();

        sub.on_event(
            &Event::new(EventKind::TaskFinished)
                .with_task("t")
                .with_outcome_kind(TaskOutcomeKind::Completed),
        );

        let text = metrics_text(&registry);
        assert!(text.contains("solti_taskvisor_task_final_outcomes_total"));
        assert!(!text.contains("solti_taskvisor_attempts_to_finalize"));
    }

    #[test]
    fn in_flight_does_not_go_negative() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::AttemptSucceeded).with_task("t"));

        assert_eq!(sub.attempts_in_flight.get(), 0.0);
    }

    #[test]
    fn subscriber_overflow_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::SubscriberOverflow)
                .with_task("t")
                .with_reason("queue full"),
        );

        assert_eq!(sub.subscriber_overflows.get(), 1.0);
    }

    #[test]
    fn subscriber_panicked_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::SubscriberPanicked)
                .with_task("t")
                .with_reason("boom"),
        );

        assert_eq!(sub.subscriber_panics.get(), 1.0);
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
        assert_eq!(sub.subscriber_panics.get(), 0.0);
    }

    #[cfg(feature = "taskvisor-controller")]
    #[test]
    fn controller_submitted_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(&Event::new(EventKind::ControllerSubmitted).with_task("t"));

        assert_eq!(sub.controller_submitted_events.get(), 1.0);
    }

    #[cfg(feature = "taskvisor-controller")]
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

    #[cfg(feature = "taskvisor-controller")]
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
                .with_label_values(&["queue_full"])
                .get(),
            1.0
        );
    }

    #[cfg(feature = "taskvisor-controller")]
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
            (RejectionKind::QueueFull, "queue_full"),
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
        assert_eq!(sub.queue_capacity(), DEFAULT_TASKVISOR_QUEUE_CAPACITY);
    }

    #[test]
    fn queue_capacity_is_overridable_via_constructor() {
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
