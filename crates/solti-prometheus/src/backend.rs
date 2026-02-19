use std::sync::Arc;

use prometheus::{CounterVec, HistogramVec, Opts, Registry, proto::MetricFamily};

use solti_core::{MetricsBackend, TaskOutcome};

/// Prometheus metrics backend for solti.
///
/// Implements [`MetricsBackend`] and exposes prometheus metrics that can be scraped via HTTP endpoint.
///
/// ## Metrics
/// - `solti_tasks_started_total{runner_type}` - Counter of spawned tasks
/// - `solti_tasks_completed_total{runner_type, outcome}` - Counter of completed tasks
/// - `solti_task_duration_seconds{runner_type}` - Histogram of task execution time
/// - `solti_runner_errors_total{runner_type, error_kind}` - Counter of runner errors
///
/// ## Label cardinality
/// All labels are bounded (low cardinality):
/// - `runner_type`: "subprocess", "wasm", "container"
/// - `outcome`: "success", "failure", "canceled", "timeout"
/// - `error_kind`: "spawn_failed", "backend_config_failed", etc
#[derive(Clone)]
pub struct PrometheusMetrics {
    tasks_started: CounterVec,
    tasks_completed: CounterVec,
    tasks_duration: HistogramVec,
    runner_errors: CounterVec,
    registry: Arc<Registry>,
}

impl PrometheusMetrics {
    /// Create a new prometheus metrics backend with custom registry.
    pub fn new_with_registry(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        let tasks_started = CounterVec::new(
            Opts::new("solti_tasks_started_total", "Total number of tasks started").namespace("solti"),
            &["runner_type"],
        )?;
        registry.register(Box::new(tasks_started.clone()))?;

        let tasks_completed = CounterVec::new(
            Opts::new(
                "solti_tasks_completed_total",
                "Total number of tasks completed",
            )
            .namespace("solti"),
            &["runner_type", "outcome"],
        )?;
        registry.register(Box::new(tasks_completed.clone()))?;

        let tasks_duration = HistogramVec::new(
            prometheus::HistogramOpts::new(
                "solti_task_duration_seconds",
                "Task execution duration in seconds",
            )
            .namespace("solti")
            .buckets(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0]),
            &["runner_type"],
        )?;
        registry.register(Box::new(tasks_duration.clone()))?;

        let runner_errors = CounterVec::new(
            Opts::new("solti_runner_errors_total", "Total runner-level errors").namespace("solti"),
            &["runner_type", "error_kind"],
        )?;
        registry.register(Box::new(runner_errors.clone()))?;

        Ok(Self {
            tasks_started,
            tasks_completed,
            tasks_duration,
            runner_errors,
            registry,
        })
    }

    /// Create a new prometheus metrics backend with default registry.
    pub fn new() -> Result<Self, prometheus::Error> {
        Self::new_with_registry(Arc::new(Registry::new()))
    }

    /// Gather all metrics for exposition.
    ///
    /// Use this to implement `/metrics` HTTP endpoint.
    ///
    /// # Example
    /// ```rust,ignore
    /// let metrics = PrometheusMetrics::new()?;
    /// let metrics_families = metrics.gather();
    /// let encoder = prometheus::TextEncoder::new();
    /// encoder.encode(&metrics_families, &mut buffer)?;
    /// ```
    pub fn gather(&self) -> Vec<MetricFamily> {
        self.registry.gather()
    }

    /// Get reference to underlying prometheus registry.
    ///
    /// Useful for registering custom metrics alongside solti metrics.
    #[allow(dead_code)]
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

impl MetricsBackend for PrometheusMetrics {
    fn record_task_started(&self, runner_type: &str) {
        self.tasks_started.with_label_values(&[runner_type]).inc();
    }

    fn record_task_completed(&self, runner_type: &str, outcome: TaskOutcome, duration_ms: u64) {
        self.tasks_completed
            .with_label_values(&[runner_type, outcome.as_label()])
            .inc();

        let duration_seconds = duration_ms as f64 / 1000.0;
        self.tasks_duration
            .with_label_values(&[runner_type])
            .observe(duration_seconds);
    }

    fn record_runner_error(&self, runner_type: &str, error_kind: &str) {
        self.runner_errors
            .with_label_values(&[runner_type, error_kind])
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

        metrics.record_task_started("subprocess");
        metrics.record_task_started("subprocess");
        metrics.record_task_started("wasm");

        let families = metrics.gather();
        let started = families
            .iter()
            .find(|f| f.name() == "solti_tasks_started_total")
            .expect("metric not found");

        assert_eq!(started.get_metric().len(), 2);
    }

    #[test]
    fn record_task_completed_increments_counter_and_histogram() {
        let metrics = PrometheusMetrics::new().unwrap();

        metrics.record_task_completed("subprocess", TaskOutcome::Success, 150);
        metrics.record_task_completed("subprocess", TaskOutcome::Failure, 50);

        let families = metrics.gather();

        let completed = families
            .iter()
            .find(|f| f.name() == "solti_tasks_completed_total")
            .expect("completed counter not found");
        assert_eq!(completed.get_metric().len(), 2);

        let duration = families
            .iter()
            .find(|f| f.name() == "solti_task_duration_seconds")
            .expect("duration histogram not found");
        assert_eq!(duration.get_metric().len(), 1);
    }

    #[test]
    fn record_runner_error_increments_counter() {
        let metrics = PrometheusMetrics::new().unwrap();

        metrics.record_runner_error("subprocess", "spawn_failed");
        metrics.record_runner_error("subprocess", "spawn_failed");
        metrics.record_runner_error("wasm", "module_load_failed");

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

        metrics.record_task_started("test");
        assert!(!registry.gather().is_empty());
    }
}
