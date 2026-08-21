//! # Runner metrics
//!
//! [`MetricsBackend`] records runner setup and cleanup errors.
//! [`RunnerType`] and [`RunnerErrorKind`] provide metric label values.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use crate::callback::{CallbackPanicFuse, dispose_panic_payload, report_without_unwind};

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
/// - `solti-prometheus::PrometheusRunnerMetrics`
/// - [`NoOpMetrics`](super::NoOpMetrics)
/// - [`crate::BuildContext::metrics`]
///
/// SDK-owned runner paths invoke this trait through [`crate::record_runner_error`]. That boundary
/// catches unwinding backend panics, discards their opaque payloads, and reports the failure
/// without unwinding. Direct application calls to this trait method are not mediated by that
/// boundary. The process panic hook still runs before the unwind is caught. A process built with
/// `panic = "abort"` cannot isolate a backend panic.
/// If destroying a hostile payload itself panics, that replacement payload is intentionally
/// forgotten to prevent another unwind.
/// Calls that already entered an installed sticky boundary concurrently may still finish or
/// panic. The boundary prevents later invocations; it does not serialize healthy callbacks.
/// Implementations must not panic; SDK containment is a defensive boundary.
pub trait MetricsBackend: Send + Sync + 'static {
    /// Records a runner error during task setup or cleanup.
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind);
}

/// Shared handle to metrics backend.
///
/// [`crate::BuildContext`] stores this handle.
pub type MetricsHandle = Arc<dyn MetricsBackend>;

/// Records a runner error without allowing a metrics backend panic to unwind.
///
/// A panicking update is dropped. Later updates may invoke a raw backend again.
/// Backends installed through [`crate::BuildContext`] share a sticky panic fuse and are not
/// invoked again after the first observed panic.
pub fn record_runner_error(
    metrics: &MetricsHandle,
    runner_type: RunnerType,
    error_kind: RunnerErrorKind,
) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
        metrics.record_runner_error(runner_type, error_kind);
    })) {
        dispose_panic_payload(payload);
        report_without_unwind(|| {
            tracing::error!(
                event = "runner.metrics_callback_panicked",
                error_kind = "callback_panicked",
                "runner metrics callback panicked; dropping this update"
            );
        });
    }
}

/// Installs one sticky panic boundary around an application metrics backend.
pub(crate) fn panic_contained_metrics(metrics: MetricsHandle) -> MetricsHandle {
    Arc::new(PanicContainedMetrics {
        metrics,
        panic_fuse: CallbackPanicFuse::default(),
    })
}

struct PanicContainedMetrics {
    metrics: MetricsHandle,
    panic_fuse: CallbackPanicFuse,
}

impl MetricsBackend for PanicContainedMetrics {
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind) {
        if self.panic_fuse.is_disabled() {
            return;
        }

        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
            self.metrics.record_runner_error(runner_type, error_kind);
        })) {
            let report = self.panic_fuse.trip();
            dispose_panic_payload(payload);
            if report {
                report_without_unwind(|| {
                    tracing::error!(
                        event = "runner.metrics_callback_panicked",
                        error_kind = "callback_panicked",
                        "runner metrics callback panicked; disabling the installed backend"
                    );
                });
            }
        }
    }
}

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
