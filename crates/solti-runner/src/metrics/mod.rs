//! # Metrics collection.
//!
//! Provides the [`MetricsBackend`] trait and a zero-cost [`NoOpMetrics`] default.
//! Concrete backends, such as `solti-prometheus`, implement this trait.
//!
//! ## Contents
//!
//! - [`RunnerErrorKind`]: metric label enum: `CgroupPrepareFailed`, `BackendConfigFailed`, `SpawnFailed`, `ModuleLoadFailed`.
//! - [`MetricsBackend`]: trait with `record_task_started`, `record_task_completed`, `record_runner_error`.
//! - [`MetricOutcome`]: metric label enum: `Success`, `Failure`, `Canceled`, `Timeout`.
//! - [`MetricsHandle`]: `Arc<dyn MetricsBackend>`, cloneable shared handle.
//! - [`RunnerType`]: metric label enum: `Subprocess`, `Wasm`, `Container`.
//! - [`noop_metrics`]: convenience constructor for `Arc<NoOpMetrics>`.
//! - [`NoOpMetrics`]: zero-size backend that discards all records.
mod backend;
pub use backend::{MetricOutcome, MetricsBackend, MetricsHandle, RunnerErrorKind, RunnerType};

mod noop;
pub use noop::NoOpMetrics;

use std::sync::Arc;

/// Create a no-op metrics handle.
///
/// ## Example
///
/// ```
/// use solti_runner::{MetricOutcome, RunnerType, noop_metrics};
///
/// let metrics = noop_metrics();
/// metrics.record_task_started(RunnerType::Subprocess);
/// metrics.record_task_completed(RunnerType::Subprocess, MetricOutcome::Success, 10);
/// ```
#[inline]
pub fn noop_metrics() -> MetricsHandle {
    Arc::new(NoOpMetrics)
}
