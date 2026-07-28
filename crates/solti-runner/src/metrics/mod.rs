//! # Metrics collection
//!
//! [`MetricsBackend`] is the runner metrics port.
//! Runners record setup and cleanup failures through this port.
//!
//! ## Flow
//!
//! ```text
//! Runner
//!    └── RunnerType + RunnerErrorKind
//!                       ▼
//!                MetricsBackend
//! ```
//!
//! Task lifecycle metrics come from taskvisor events.
//! [`NoOpMetrics`] is the default runner backend.
//! `solti-prometheus` provides a Prometheus implementation.
mod backend;
pub use backend::{MetricsBackend, MetricsHandle, RunnerErrorKind, RunnerType};

mod noop;
pub use noop::NoOpMetrics;

use std::sync::Arc;

/// Creates a shared no-op metrics handle.
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
