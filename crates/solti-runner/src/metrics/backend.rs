//! # Runner metrics
//!
//! [`MetricsBackend`] records runner setup and cleanup errors.
//! [`RunnerType`] and [`RunnerErrorKind`] provide metric label values.

use std::sync::Arc;

/// Runner implementation type for metrics labeling.
///
/// Built-in variants return fixed labels.
/// [`Custom`](Self::Custom) returns its string unchanged.
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
    /// Returns the metric label value.
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
/// Built-in variants return fixed labels.
/// [`Custom`](Self::Custom) returns its string unchanged.
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
    /// Returns the metric label value.
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
/// Implementations record runner setup and cleanup failures.
/// Task lifecycle metrics come from taskvisor events.
///
/// ```text
/// runner failure
///       ├── RunnerType
///       └── RunnerErrorKind
///                 ▼
///       record_runner_error
/// ```
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
/// ## See Also
///
/// - [`NoOpMetrics`](super::NoOpMetrics)
/// - [`crate::BuildContext::metrics`]
/// - `solti-prometheus::PrometheusRunnerMetrics`
pub trait MetricsBackend: Send + Sync + 'static {
    /// Records a runner error during task setup or cleanup.
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind);
}

/// Shared handle to metrics backend.
///
/// [`crate::BuildContext`] stores this handle.
pub type MetricsHandle = Arc<dyn MetricsBackend>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_labels_are_stable_for_built_in_and_custom_variants() {
        for (runner_type, expected) in [
            (RunnerType::Subprocess, "subprocess"),
            (RunnerType::Container, "container"),
            (RunnerType::Wasm, "wasm"),
            (RunnerType::Custom("image-resize".into()), "image-resize"),
        ] {
            assert_eq!(runner_type.as_label(), expected);
        }

        for (error_kind, expected) in [
            (
                RunnerErrorKind::CgroupPrepareFailed,
                "cgroup_prepare_failed",
            ),
            (
                RunnerErrorKind::BackendConfigFailed,
                "backend_config_failed",
            ),
            (RunnerErrorKind::SpawnFailed, "spawn_failed"),
            (RunnerErrorKind::ModuleLoadFailed, "module_load_failed"),
            (
                RunnerErrorKind::Custom("runtime_unavailable".into()),
                "runtime_unavailable",
            ),
        ] {
            assert_eq!(error_kind.as_label(), expected);
        }
    }
}
