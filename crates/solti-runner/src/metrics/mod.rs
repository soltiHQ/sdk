//! # Metrics collection abstraction.
//!
//! Provides [`MetricsBackend`] trait and a zero-cost [`NoOpMetrics`] default.
//! Concrete backends (e.g. `solti-prometheus`) implement the trait; the active backend is injected via [`BuildContext`](crate::BuildContext).
//!
//! ## Contents
//!
//! - [`RunnerErrorKind`]: metric label enum: `CgroupPrepareFailed`, `BackendConfigFailed`, `SpawnFailed`, `ModuleLoadFailed`.
//! - [`MetricsBackend`]: trait with `record_task_started`, `record_task_completed`, `record_runner_error`.
//! - [`MetricOutcome`]: metric label enum: `Success`, `Failure`, `Canceled`, `Timeout`.
//! - [`NoOpMetrics`]: zero-size backend (`#[inline(always)]`, compiles to nothing).
//! - [`MetricsHandle`]: `Arc<dyn MetricsBackend>`, cloneable shared handle.
//! - [`RunnerType`]: metric label enum: `Subprocess`, `Wasm`, `Container`.
//! - [`noop_metrics`]: convenience constructor for `Arc<NoOpMetrics>`.
mod backend;
pub use backend::{MetricOutcome, MetricsBackend, MetricsHandle, RunnerErrorKind, RunnerType};

mod noop;
pub use noop::NoOpMetrics;

use std::sync::Arc;

/// Create a no-op metrics handle.
#[inline]
pub fn noop_metrics() -> MetricsHandle {
    Arc::new(NoOpMetrics)
}
