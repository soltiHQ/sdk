//! # Runner Prometheus metrics
//!
//! [`PrometheusRunnerMetrics`] implements [`MetricsBackend`] for Solti runners.
//! It records setup and cleanup errors.
//!
//! Enable it with the `runner` feature.
//!
//! ## Flow
//!
//! ```text
//! Runner ──► MetricsBackend ──► PrometheusRunnerMetrics ──► Registry
//! ```

use prometheus::{CounterVec, Registry};

use solti_runner::{MetricsBackend, RunnerErrorKind, RunnerType};

use crate::register::MetricGroup;

/// Prometheus runner metrics.
///
/// ## Metrics
///
/// | Metric                      | Type    | Labels            | Description              |
/// |-----------------------------|---------|-------------------|--------------------------|
/// | `solti_runner_errors_total` | Counter | `runner`, `error` | Setup and cleanup errors |
///
/// ## Labels
///
/// Built-in variants use stable labels from [`RunnerType`] and [`RunnerErrorKind`].
/// Custom variants use the application-provided string unchanged.
///
/// ```text
/// RunnerType ──────► runner label
/// RunnerErrorKind ─► error label
/// ```
///
/// The application controls cardinality for custom runner and error labels.
///
/// ## Example
///
/// ```rust
/// use solti_prometheus::{PrometheusRunnerMetrics, Registry};
/// use solti_runner::{MetricsBackend, RunnerErrorKind, RunnerType};
///
/// let registry = Registry::new();
/// let metrics = PrometheusRunnerMetrics::new(&registry).unwrap();
/// metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
///
/// assert!(!registry.gather().is_empty());
/// ```
pub struct PrometheusRunnerMetrics {
    runner_errors: CounterVec,
}

impl PrometheusRunnerMetrics {
    /// Creates a runner metrics backend and registers its collector.
    ///
    /// The returned backend updates the collector in `registry`.
    ///
    /// # Errors
    ///
    /// Returns a Prometheus error when the collector cannot be created or registered.
    /// A descriptor conflict returns [`prometheus::Error::AlreadyReg`].
    pub fn new(registry: &Registry) -> Result<Self, prometheus::Error> {
        let mut metrics = MetricGroup::new();

        let runner_errors = metrics.counter_vec(
            "runner",
            "errors_total",
            "Total runner-level errors",
            &["runner", "error"],
        )?;
        metrics.register(registry)?;

        Ok(Self { runner_errors })
    }
}

impl std::fmt::Debug for PrometheusRunnerMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusRunnerMetrics").finish()
    }
}

impl MetricsBackend for PrometheusRunnerMetrics {
    /// Increments `solti_runner_errors_total{runner=<runner_type>, error=<error_kind>}`.
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind) {
        self.runner_errors
            .with_label_values(&[runner_type.as_label(), error_kind.as_label()])
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_metrics() -> (Registry, PrometheusRunnerMetrics) {
        let registry = Registry::new();
        let metrics = PrometheusRunnerMetrics::new(&registry).expect("failed to create metrics");
        (registry, metrics)
    }

    #[test]
    fn record_runner_error_increments_counter() {
        let (registry, metrics) = new_metrics();

        metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        metrics.record_runner_error(RunnerType::Wasm, RunnerErrorKind::ModuleLoadFailed);

        let families = registry.gather();
        let errors = families
            .iter()
            .find(|f| f.name() == "solti_runner_errors_total")
            .expect("errors counter not found");

        assert_eq!(errors.get_metric().len(), 2);
    }

    #[test]
    fn custom_runner_type_is_exported_as_its_label() {
        let (registry, metrics) = new_metrics();
        metrics.record_runner_error(
            RunnerType::Custom("image-resize".into()),
            RunnerErrorKind::Custom("runtime_unavailable".into()),
        );

        let families = registry.gather();
        let errors = families
            .iter()
            .find(|family| family.name() == "solti_runner_errors_total")
            .unwrap();
        let labels = errors.get_metric()[0].get_label();
        assert!(
            labels
                .iter()
                .any(|label| { label.name() == "runner" && label.value() == "image-resize" })
        );
        assert!(
            labels
                .iter()
                .any(|label| { label.name() == "error" && label.value() == "runtime_unavailable" })
        );
    }
}
