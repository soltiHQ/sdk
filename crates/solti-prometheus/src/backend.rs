//! # Runner-level Prometheus metrics.
//!
//! [`PrometheusMetrics`] implements [`MetricsBackend`] and exposes counters and histograms for task execution events reported by runners.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::sync::Arc;

use prometheus::{CounterVec, HistogramVec, Registry, proto::MetricFamily};

use solti_runner::{MetricsBackend, RunnerType, TaskOutcome};

use crate::register::{Sub, ms_to_secs};

/// Prometheus metrics backend for solti runners.
///
/// Implements [`MetricsBackend`] and exposes runner-level metrics in Prometheus format.
/// Runners call the trait methods during task lifecycle.
///
/// ## Metrics
///
/// | Metric                               | Type      | Labels              | Description                    |
/// |--------------------------------------|-----------|---------------------|--------------------------------|
/// | `solti_runner_tasks_started_total`   | Counter   | `runner`            | Task spawn events              |
/// | `solti_runner_tasks_completed_total` | Counter   | `runner`, `outcome` | Task completion events         |
/// | `solti_runner_task_duration_seconds` | Histogram | `runner`, `outcome` | Per-attempt execution duration |
/// | `solti_runner_errors_total`          | Counter   | `runner`, `error`   | Runner setup/teardown errors   |
///
/// ## Labels
///
/// All label sets have low, bounded cardinality:
///
/// | Label     | Values                                      | Cardinality |
/// |-----------|---------------------------------------------|-------------|
/// | `runner`  | `subprocess`, `wasm`, `container`           | Low         |
/// | `outcome` | `success`, `failure`, `canceled`, `timeout` | Low         |
/// | `error`   | `spawn_failed`, `backend_config_failed`, …  | Low         |
///
/// ## Duration histogram buckets
///
/// Buckets (seconds): `0.01, 0.05, 0.1, 0.5, 1, 5, 10, 30, 60, 120, 300, 600, 1800, 3600`.
/// Covers sub-second scripts up to 1-hour long-running tasks.
///
/// ## Also
///
/// - [`PrometheusSubscriber`](crate::PrometheusSubscriber) is a supervision-level metrics from the event stream.
/// - [`Registry`](prometheus::Registry) is a shared registry for unified `/metrics` endpoint.
#[derive(Clone)]
pub struct PrometheusMetrics {
    tasks_started: CounterVec,
    tasks_completed: CounterVec,
    tasks_duration: HistogramVec,
    runner_errors: CounterVec,
    registry: Arc<Registry>,
}

impl PrometheusMetrics {
    /// Create a new metrics backend, registering all counters and histograms into the given [`Registry`].
    pub fn new_with_registry(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        let r = Sub::new(&registry, "runner");

        let tasks_started = r.counter_vec(
            "tasks_started_total",
            "Total number of tasks started",
            &["runner"],
        )?;
        let tasks_completed = r.counter_vec(
            "tasks_completed_total",
            "Total number of tasks completed",
            &["runner", "outcome"],
        )?;
        let tasks_duration = r.histogram_vec(
            "task_duration_seconds",
            "Task execution duration in seconds",
            vec![
                0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
                3600.0,
            ],
            &["runner", "outcome"],
        )?;
        let runner_errors = r.counter_vec(
            "errors_total",
            "Total runner-level errors",
            &["runner", "error"],
        )?;

        Ok(Self {
            tasks_started,
            tasks_completed,
            tasks_duration,
            runner_errors,
            registry,
        })
    }

    /// Create a new metrics backend with an **isolated** registry.
    pub fn new() -> Result<Self, prometheus::Error> {
        Self::new_with_registry(Arc::new(Registry::new()))
    }

    /// Gather all metrics for exposition.
    pub fn gather(&self) -> Vec<MetricFamily> {
        self.registry.gather()
    }

    /// Get reference to underlying prometheus registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

impl MetricsBackend for PrometheusMetrics {
    /// Increments `solti_runner_tasks_started_total{runner=<runner_type>}`.
    fn record_task_started(&self, runner_type: RunnerType) {
        self.tasks_started
            .with_label_values(&[runner_type.as_label()])
            .inc();
    }

    /// Records a task completion event.
    ///
    /// Updates two metrics:
    /// - `solti_runner_tasks_completed_total{runner, outcome}` - incremented by 1.
    /// - `solti_runner_task_duration_seconds{runner, outcome}` - observes the duration converted from milliseconds to seconds.
    ///
    /// The `outcome` label is derived from [`TaskOutcome::as_label`]: `success` | `failure` | `canceled` | `timeout`.
    fn record_task_completed(
        &self,
        runner_type: RunnerType,
        outcome: TaskOutcome,
        duration_ms: u64,
    ) {
        let runner = runner_type.as_label();
        let label = outcome.as_label();

        self.tasks_completed
            .with_label_values(&[runner, label])
            .inc();
        self.tasks_duration
            .with_label_values(&[runner, label])
            .observe(ms_to_secs(duration_ms));
    }

    /// Increments `solti_runner_errors_total{runner=<runner_type>, error=<error_kind>}`.
    ///
    /// Called for runner setup/teardown errors (e.g. spawn failures), **not** for task-level failures
    /// which go through [`record_task_completed`](MetricsBackend::record_task_completed).
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: &str) {
        self.runner_errors
            .with_label_values(&[runner_type.as_label(), error_kind])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_create_prometheus_metrics() {
        let _metrics = PrometheusMetrics::new().expect("failed to create metrics");
    }

    #[test]
    fn record_task_started_increments_counter() {
        let metrics = PrometheusMetrics::new().unwrap();

        metrics.record_task_started(RunnerType::Subprocess);
        metrics.record_task_started(RunnerType::Subprocess);
        metrics.record_task_started(RunnerType::Wasm);

        let families = metrics.gather();
        let started = families
            .iter()
            .find(|f| f.name() == "solti_runner_tasks_started_total")
            .expect("metric not found");

        assert_eq!(started.get_metric().len(), 2);
    }

    #[test]
    fn record_task_completed_increments_counter_and_histogram() {
        let metrics = PrometheusMetrics::new().unwrap();

        metrics.record_task_completed(RunnerType::Subprocess, TaskOutcome::Success, 150);
        metrics.record_task_completed(RunnerType::Subprocess, TaskOutcome::Failure, 50);

        let families = metrics.gather();

        let completed = families
            .iter()
            .find(|f| f.name() == "solti_runner_tasks_completed_total")
            .expect("completed counter not found");
        assert_eq!(completed.get_metric().len(), 2);

        let duration = families
            .iter()
            .find(|f| f.name() == "solti_runner_task_duration_seconds")
            .expect("duration histogram not found");
        assert_eq!(duration.get_metric().len(), 2);
    }

    #[test]
    fn record_runner_error_increments_counter() {
        let metrics = PrometheusMetrics::new().unwrap();

        metrics.record_runner_error(RunnerType::Subprocess, "spawn_failed");
        metrics.record_runner_error(RunnerType::Subprocess, "spawn_failed");
        metrics.record_runner_error(RunnerType::Wasm, "module_load_failed");

        let families = metrics.gather();
        let errors = families
            .iter()
            .find(|f| f.name() == "solti_runner_errors_total")
            .expect("errors counter not found");

        assert_eq!(errors.get_metric().len(), 2);
    }

    #[test]
    fn can_use_custom_registry() {
        let registry = Arc::new(Registry::new());
        let metrics = PrometheusMetrics::new_with_registry(registry.clone()).unwrap();

        metrics.record_task_started(RunnerType::Subprocess);
        assert!(!registry.gather().is_empty());
    }
}
