//! # Metrics backend trait and label types
//!
//! [`MetricsBackend`] records runner backend errors.
//! Concrete backends, such as `solti-prometheus`, implement this trait.
//!
//! See the [metrics module](super) for the convenience [`noop_metrics`](super::noop_metrics) constructor.

use std::sync::Arc;

/// Runner implementation type for metrics labeling.
///
/// Passed to [`MetricsBackend`] methods so dashboards can slice metrics by runner backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunnerType {
    /// OS subprocess runner.
    Subprocess,
    /// Container (OCI) runner.
    Container,
    /// WebAssembly runner.
    Wasm,
    /// Application-defined runner label.
    Custom(String),
}

impl RunnerType {
    /// Return the stable label value for metrics.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_runner::RunnerType;
    ///
    /// assert_eq!(RunnerType::Subprocess.as_label(), "subprocess");
    /// ```
    #[inline]
    pub fn as_label(&self) -> &str {
        match self {
            Self::Subprocess => "subprocess",
            Self::Container => "container",
            Self::Wasm => "wasm",
            Self::Custom(label) => label,
        }
    }
}

/// Runner setup or teardown error kind for metrics labeling.
///
/// Passed to [`MetricsBackend::record_runner_error`]; dashboards can use a bounded label instead of free-form error strings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RunnerErrorKind {
    /// cgroup v2 preparation (creation / attribute write) failed.
    CgroupPrepareFailed,
    /// Applying runner-specific configuration to the task command failed.
    BackendConfigFailed,
    /// Spawning the child process or actor failed.
    SpawnFailed,
    /// Loading the runner module (WASM / container image) failed.
    ModuleLoadFailed,
    /// Application-defined stable error label.
    Custom(String),
}

impl RunnerErrorKind {
    /// Return the stable label value for metrics.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_runner::RunnerErrorKind;
    ///
    /// assert_eq!(RunnerErrorKind::SpawnFailed.as_label(), "spawn_failed");
    /// ```
    #[inline]
    pub fn as_label(&self) -> &str {
        match self {
            Self::CgroupPrepareFailed => "cgroup_prepare_failed",
            Self::BackendConfigFailed => "backend_config_failed",
            Self::SpawnFailed => "spawn_failed",
            Self::ModuleLoadFailed => "module_load_failed",
            Self::Custom(label) => label,
        }
    }
}

/// Backend metrics collection interface.
///
/// Implementations record runner-specific setup and cleanup failures.
/// Task lifecycle metrics come from taskvisor events.
///
/// ## Example
///
/// ```
/// use std::sync::atomic::{AtomicU64, Ordering};
/// use solti_runner::{MetricsBackend, RunnerErrorKind, RunnerType};
///
/// #[derive(Default)]
/// struct Counter {
///     errors: AtomicU64,
/// }
///
/// impl MetricsBackend for Counter {
///     fn record_runner_error(&self, _runner_type: RunnerType, _error_kind: RunnerErrorKind) {
///         self.errors.fetch_add(1, Ordering::Relaxed);
///     }
/// }
///
/// let metrics = Counter::default();
/// metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
/// assert_eq!(metrics.errors.load(Ordering::Relaxed), 1);
/// ```
///
/// ## Also
///
/// - [`NoOpMetrics`](super::NoOpMetrics): zero-size default backend.
/// - [`crate::BuildContext::metrics`]: access the handle from within a runner.
/// - `solti-prometheus::PrometheusRunnerMetrics` is a production Prometheus implementation.
pub trait MetricsBackend: Send + Sync + 'static {
    /// Record a runner-specific error during task setup or cleanup.
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind);
}

/// Shared handle to metrics backend.
///
/// Stored in [`crate::BuildContext`] and cloned into each task.
pub type MetricsHandle = Arc<dyn MetricsBackend>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_error_kind_as_label_maps_all_variants() {
        assert_eq!(
            RunnerErrorKind::CgroupPrepareFailed.as_label(),
            "cgroup_prepare_failed"
        );
        assert_eq!(
            RunnerErrorKind::BackendConfigFailed.as_label(),
            "backend_config_failed"
        );
        assert_eq!(RunnerErrorKind::SpawnFailed.as_label(), "spawn_failed");
        assert_eq!(
            RunnerErrorKind::ModuleLoadFailed.as_label(),
            "module_load_failed"
        );
        assert_eq!(
            RunnerErrorKind::Custom("runtime_unavailable".into()).as_label(),
            "runtime_unavailable"
        );
    }

    #[test]
    fn custom_runner_type_preserves_label() {
        let runner_type = RunnerType::Custom("image-resize".into());
        assert_eq!(runner_type.as_label(), "image-resize");
    }
}
