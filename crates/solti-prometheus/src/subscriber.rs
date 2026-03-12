use std::sync::Arc;

use async_trait::async_trait;
use prometheus::{Counter, CounterVec, Gauge, Histogram, Opts, Registry};
use taskvisor::{BackoffSource, Event, EventKind, Subscribe};
use tracing::debug;

/// Prometheus subscriber for supervision-level metrics.
///
/// Implements [`Subscribe`] and captures metrics from the taskvisor event stream.
/// Must share a [`Registry`] with [`crate::PrometheusMetrics`] for a unified `/metrics` endpoint.
///
/// ## Supervision metrics (`solti_sv_*`)
///
/// | Metric | Type | Labels | Description |
/// |------------------------------------------|-----------|----------|---------------------------|
/// | `solti_sv_tasks_in_flight`               | Gauge     | -        | Currently executing tasks |
/// | `solti_sv_task_restarts_total`           | Counter   | -        | Restarts (attempt > 1)    |
/// | `solti_sv_task_backoff_count_total`      | Counter   | `source` | Backoff events            |
/// | `solti_sv_task_backoff_duration_seconds` | Histogram | -        | Backoff delay duration    |
/// | `solti_sv_task_terminal_total`           | Counter   | `reason` | Terminal task states      |
/// | `solti_sv_task_timeouts_total`           | Counter   | -        | Timeout events            |
///
/// ## Controller metrics (`solti_ctrl_*`, feature `controller`)
///
/// | Metric                         | Type    | Labels | Description            |
/// |--------------------------------|---------|--------|------------------------|
/// | `solti_ctrl_submissions_total` | Counter | -      | Controller submissions |
/// | `solti_ctrl_rejections_total`  | Counter | —      | Controller rejections  |
pub struct PrometheusSubscriber {
    task_backoff_duration: Histogram,
    task_backoff_count: CounterVec,
    task_terminal: CounterVec,
    task_timeouts: Counter,
    task_restarts: Counter,
    tasks_in_flight: Gauge,

    #[cfg(feature = "controller")]
    controller_submissions: Counter,
    #[cfg(feature = "controller")]
    controller_rejections: Counter,
}

impl PrometheusSubscriber {
    /// Create a new subscriber registering metrics into the given registry.
    pub fn new(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        let tasks_in_flight = Gauge::with_opts(
            Opts::new("tasks_in_flight", "Number of tasks currently executing")
                .namespace("solti")
                .subsystem("sv"),
        )?;
        registry.register(Box::new(tasks_in_flight.clone()))?;

        let task_restarts = Counter::with_opts(
            Opts::new("task_restarts_total", "Total task restarts (attempt > 1)")
                .namespace("solti")
                .subsystem("sv"),
        )?;
        registry.register(Box::new(task_restarts.clone()))?;

        let task_backoff_count = CounterVec::new(
            Opts::new("task_backoff_count_total", "Total backoff events")
                .namespace("solti")
                .subsystem("sv"),
            &["source"],
        )?;
        registry.register(Box::new(task_backoff_count.clone()))?;

        let task_backoff_duration = Histogram::with_opts(
            prometheus::HistogramOpts::new(
                "task_backoff_duration_seconds",
                "Backoff delay duration in seconds",
            )
            .namespace("solti")
            .subsystem("sv")
            .buckets(vec![
                0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
            ]),
        )?;
        registry.register(Box::new(task_backoff_duration.clone()))?;

        let task_terminal = CounterVec::new(
            Opts::new("task_terminal_total", "Total terminal task states")
                .namespace("solti")
                .subsystem("sv"),
            &["reason"],
        )?;
        registry.register(Box::new(task_terminal.clone()))?;

        let task_timeouts = Counter::with_opts(
            Opts::new("task_timeouts_total", "Total task timeout events")
                .namespace("solti")
                .subsystem("sv"),
        )?;
        registry.register(Box::new(task_timeouts.clone()))?;

        #[cfg(feature = "controller")]
        let controller_submissions = {
            let c = Counter::with_opts(
                Opts::new("submissions_total", "Total controller submissions")
                    .namespace("solti")
                    .subsystem("ctrl"),
            )?;
            registry.register(Box::new(c.clone()))?;
            c
        };

        #[cfg(feature = "controller")]
        let controller_rejections = {
            let c = Counter::with_opts(
                Opts::new("rejections_total", "Total controller rejections")
                    .namespace("solti")
                    .subsystem("ctrl"),
            )?;
            registry.register(Box::new(c.clone()))?;
            c
        };

        Ok(Self {
            tasks_in_flight,
            task_restarts,
            task_backoff_count,
            task_backoff_duration,
            task_terminal,
            task_timeouts,
            #[cfg(feature = "controller")]
            controller_submissions,
            #[cfg(feature = "controller")]
            controller_rejections,
        })
    }
}

#[async_trait]
impl Subscribe for PrometheusSubscriber {
    async fn on_event(&self, event: &Event) {
        match event.kind {
            EventKind::TaskStarting => {
                self.tasks_in_flight.inc();
                if event.attempt.unwrap_or(1) > 1 {
                    self.task_restarts.inc();
                }
            }
            EventKind::TaskStopped | EventKind::TaskFailed => {
                self.tasks_in_flight.dec();
            }
            EventKind::TimeoutHit => {
                self.task_timeouts.inc();
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
                        .observe(f64::from(delay_ms) / 1000.0);
                }
            }
            EventKind::ActorExhausted => {
                self.task_terminal.with_label_values(&["exhausted"]).inc();
            }
            EventKind::ActorDead => {
                self.task_terminal.with_label_values(&["fatal"]).inc();
            }
            #[cfg(feature = "controller")]
            EventKind::ControllerSubmitted => {
                self.controller_submissions.inc();
            }
            #[cfg(feature = "controller")]
            EventKind::ControllerRejected => {
                self.controller_rejections.inc();
            }
            other => {
                debug!(kind = ?other, "unhandled event kind");
            }
        }
    }

    fn name(&self) -> &'static str {
        "prometheus"
    }

    fn queue_capacity(&self) -> usize {
        2048
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Encoder;
    use solti_core::MetricsBackend;
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

    #[tokio::test]
    async fn task_starting_increments_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        )
        .await;

        assert_eq!(sub.tasks_in_flight.get(), 1.0);
    }

    #[tokio::test]
    async fn task_stopped_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        )
        .await;
        sub.on_event(&Event::new(EventKind::TaskStopped).with_task("t"))
            .await;

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[tokio::test]
    async fn task_failed_decrements_in_flight() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        )
        .await;
        sub.on_event(
            &Event::new(EventKind::TaskFailed)
                .with_task("t")
                .with_reason("boom"),
        )
        .await;

        assert_eq!(sub.tasks_in_flight.get(), 0.0);
    }

    #[tokio::test]
    async fn first_attempt_is_not_a_restart() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        )
        .await;

        assert_eq!(sub.task_restarts.get(), 0.0);
    }

    #[tokio::test]
    async fn second_attempt_is_a_restart() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(2),
        )
        .await;

        assert_eq!(sub.task_restarts.get(), 1.0);
    }

    #[tokio::test]
    async fn backoff_failure_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::BackoffScheduled)
                .with_task("t")
                .with_delay(Duration::from_secs(5))
                .with_backoff_failure(),
        )
        .await;

        assert_eq!(
            sub.task_backoff_count.with_label_values(&["failure"]).get(),
            1.0
        );
    }

    #[tokio::test]
    async fn backoff_success_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::BackoffScheduled)
                .with_task("t")
                .with_delay(Duration::from_secs(10))
                .with_backoff_success(),
        )
        .await;

        assert_eq!(
            sub.task_backoff_count.with_label_values(&["success"]).get(),
            1.0
        );
    }

    #[tokio::test]
    async fn timeout_hit_increments_counter() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::TimeoutHit)
                .with_task("t")
                .with_timeout(Duration::from_secs(30)),
        )
        .await;

        assert_eq!(sub.task_timeouts.get(), 1.0);
    }

    #[tokio::test]
    async fn actor_exhausted_increments_terminal() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorExhausted)
                .with_task("t")
                .with_reason("policy done"),
        )
        .await;

        assert_eq!(
            sub.task_terminal.with_label_values(&["exhausted"]).get(),
            1.0
        );
    }

    #[tokio::test]
    async fn actor_dead_increments_terminal() {
        let sub = new_subscriber();

        sub.on_event(
            &Event::new(EventKind::ActorDead)
                .with_task("t")
                .with_reason("fatal error"),
        )
        .await;

        assert_eq!(sub.task_terminal.with_label_values(&["fatal"]).get(), 1.0);
    }

    #[tokio::test]
    async fn shared_registry_with_backend() {
        let registry = Arc::new(Registry::new());

        let backend = crate::PrometheusMetrics::new_with_registry(registry.clone()).unwrap();
        let sub = PrometheusSubscriber::new(registry.clone()).unwrap();

        backend.record_task_started("subprocess");
        sub.on_event(
            &Event::new(EventKind::TaskStarting)
                .with_task("t")
                .with_attempt(1),
        )
        .await;

        let text = metrics_text(&registry);
        assert!(text.contains("solti_runner_tasks_started_total"));
        assert!(text.contains("solti_sv_tasks_in_flight"));
    }
}
