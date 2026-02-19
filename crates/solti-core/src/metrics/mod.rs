//! Metrics collection abstraction for solti runners.
//!
//! This module provides a backend interface for collecting runtime metrics from task execution.
//! Metrics backends (prometheus, statsd, etc) implement [`MetricsBackend`] and are injected via [`crate::BuildContext`].
mod backend;
pub use backend::{MetricsBackend, MetricsHandle, TaskOutcome};

mod noop;
pub use noop::NoOpMetrics;

use std::sync::Arc;

/// Create a no-op metrics handle.
#[inline]
pub fn noop_metrics() -> MetricsHandle {
    Arc::new(NoOpMetrics)
}
