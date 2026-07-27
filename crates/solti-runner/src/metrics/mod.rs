//! # Metrics collection
//!
//! Provides the [`MetricsBackend`] trait and a no-op default.
//! Concrete backends, such as `solti-prometheus`, implement this trait.
//!
//! ## Contents
//!
//! - [`RunnerErrorKind`]: metric label enum: `CgroupPrepareFailed`, `BackendConfigFailed`, `SpawnFailed`, `ModuleLoadFailed`.
//! - [`MetricsBackend`]: runner-specific error metrics.
//! - [`MetricsHandle`]: `Arc<dyn MetricsBackend>`, cloneable shared handle.
//! - [`RunnerType`]: built-in and application-defined runner labels.
//! - [`noop_metrics`]: convenience constructor for `Arc<NoOpMetrics>`.
//! - [`NoOpMetrics`]: zero-size backend that discards all records.
mod backend;
pub use backend::{MetricsBackend, MetricsHandle, RunnerErrorKind, RunnerType};

mod noop;
pub use noop::NoOpMetrics;

use std::sync::Arc;

/// Create a no-op metrics handle.
///
/// ## Example
///
/// ```
/// use solti_runner::{RunnerErrorKind, RunnerType, noop_metrics};
///
/// let metrics = noop_metrics();
/// metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
/// ```
#[inline]
pub fn noop_metrics() -> MetricsHandle {
    Arc::new(NoOpMetrics)
}
