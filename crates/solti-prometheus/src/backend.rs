//! # Runner Prometheus metrics.
//!
//! [`PrometheusMetrics`] implements [`MetricsBackend`] for Solti runners.
//! It exposes counters and histograms for task starts, completions, durations, and runner setup errors.
//!
//! See the [crate root](crate) for architecture and namespace overview.

use std::sync::Arc;

use prometheus::{CounterVec, HistogramVec, Registry, proto::MetricFamily};

use solti_runner::{MetricOutcome, MetricsBackend, RunnerErrorKind, RunnerType};

use crate::register::{Sub, ms_to_secs};

/// Prometheus metrics backend for Solti runners.
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
/// | Label     | Values                                                                                                            | Cardinality |
/// |-----------|-------------------------------------------------------------------------------------------------------------------|-------------|
/// | `runner`  | `subprocess`, `wasm`, `container`                                                                                 | Low         |
/// | `outcome` | `success`, `failure`, `canceled`, `timeout`                                                                       | Low         |
/// | `error`   | `cgroup_prepare_failed`, `backend_config_failed`, `spawn_failed`, `module_load_failed` (from [`RunnerErrorKind`]) | Low         |
///
/// ## Duration histogram buckets
///
/// Buckets (seconds): `0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 300, 1800, 3600`.
///
/// ## Example
///
/// ```rust
/// use solti_prometheus::PrometheusMetrics;
/// use solti_runner::{MetricsBackend, RunnerType};
///
/// // `new_isolated` owns a private registry - no shared wiring needed.
/// let metrics = PrometheusMetrics::new_isolated().unwrap();
/// metrics.record_task_started(RunnerType::Subprocess);
///
/// assert!(!metrics.gather().is_empty());
/// ```
///
/// ## Also
///
/// - [`PrometheusSubscriber`](crate::PrometheusSubscriber): supervision-level metrics from the event stream.
/// - [`Registry`](prometheus::Registry): shared registry for a unified `/metrics` endpoint.
pub struct PrometheusMetrics {
    tasks_started: CounterVec,
    tasks_completed: CounterVec,
    tasks_duration: HistogramVec,
    runner_errors: CounterVec,
    registry: Arc<Registry>,
}

impl PrometheusMetrics {
    /// Create a new metrics backend and register it into `registry`.
    ///
    /// Primary constructor - mirrors the shape used by other backends in this
    /// crate ([`PrometheusSubscriber::new`](crate::PrometheusSubscriber::new),
    /// `PrometheusApiMetrics::new`, `PrometheusDiscoverMetrics::new`).
    ///
    /// ## Example
    ///
    /// ```
    /// use std::sync::Arc;
    /// use solti_prometheus::{PrometheusMetrics, Registry};
    /// use solti_runner::{MetricsBackend, RunnerType};
    ///
    /// # fn main() -> Result<(), prometheus::Error> {
    /// let registry = Arc::new(Registry::new());
    /// let metrics = PrometheusMetrics::new(registry.clone())?;
    ///
    /// metrics.record_task_started(RunnerType::Subprocess);
    /// assert!(!registry.gather().is_empty());
    /// # Ok(()) }
    /// ```
    pub fn new(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
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
                0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 1800.0,
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

    /// Create a new metrics backend with an isolated registry.
    ///
    /// Convenience for tests / standalone use. Most agents share a single registry across collectors via [`Self::new`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_prometheus::PrometheusMetrics;
    /// use solti_runner::{MetricsBackend, RunnerType};
    ///
    /// let metrics = PrometheusMetrics::new_isolated().unwrap();
    /// metrics.record_task_started(RunnerType::Subprocess);
    ///
    /// assert!(!metrics.gather().is_empty());
    /// ```
    pub fn new_isolated() -> Result<Self, prometheus::Error> {
        Self::new(Arc::new(Registry::new()))
    }

    /// Deprecated alias of [`Self::new`].
    ///
    /// ## Errors
    ///
    /// See [`Self::new`].
    #[deprecated(
        since = "0.0.2",
        note = "use `PrometheusMetrics::new(registry)` - same signature, consistent with the other backends"
    )]
    pub fn new_with_registry(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        Self::new(registry)
    }

    /// Gather all metrics from the underlying registry.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_prometheus::PrometheusMetrics;
    /// use solti_runner::{MetricsBackend, RunnerType};
    ///
    /// let metrics = PrometheusMetrics::new_isolated().unwrap();
    /// metrics.record_task_started(RunnerType::Subprocess);
    ///
    /// let families = metrics.gather();
    /// assert!(families.iter().any(|f| f.name() == "solti_runner_tasks_started_total"));
    /// ```
    pub fn gather(&self) -> Vec<MetricFamily> {
        self.registry.gather()
    }

    /// Get the underlying Prometheus registry.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_prometheus::PrometheusMetrics;
    ///
    /// let metrics = PrometheusMetrics::new_isolated().unwrap();
    /// assert!(metrics.registry().gather().is_empty());
    /// ```
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }
}

impl std::fmt::Debug for PrometheusMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusMetrics").finish()
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
    /// The `outcome` label is derived from [`MetricOutcome::as_label`]: `success` | `failure` | `canceled` | `timeout`.
    fn record_task_completed(
        &self,
        runner_type: RunnerType,
        outcome: MetricOutcome,
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
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind) {
        self.runner_errors
            .with_label_values(&[runner_type.as_label(), error_kind.as_label()])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_create_prometheus_metrics() {
        let _metrics = PrometheusMetrics::new_isolated().expect("failed to create metrics");
    }

    #[test]
    fn record_task_started_increments_counter() {
        let metrics = PrometheusMetrics::new_isolated().unwrap();

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
        let metrics = PrometheusMetrics::new_isolated().unwrap();

        metrics.record_task_completed(RunnerType::Subprocess, MetricOutcome::Success, 150);
        metrics.record_task_completed(RunnerType::Subprocess, MetricOutcome::Failure, 50);

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
        let metrics = PrometheusMetrics::new_isolated().unwrap();

        metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        metrics.record_runner_error(RunnerType::Wasm, RunnerErrorKind::ModuleLoadFailed);

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
        let metrics = PrometheusMetrics::new(registry.clone()).unwrap();

        metrics.record_task_started(RunnerType::Subprocess);
        assert!(!registry.gather().is_empty());
    }
}
