//! # No-op metrics backend

use crate::metrics::backend::{MetricsBackend, RunnerErrorKind, RunnerType};

/// Zero-sized [`MetricsBackend`] that discards records.
///
/// [`BuildContext`](crate::BuildContext) uses it by default.
///
/// ## Example
///
/// ```
/// use solti_runner::{MetricsBackend, NoOpMetrics, RunnerErrorKind, RunnerType};
///
/// let metrics = NoOpMetrics;
/// metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpMetrics;

impl MetricsBackend for NoOpMetrics {
    #[inline(always)]
    fn record_runner_error(&self, _: RunnerType, _: RunnerErrorKind) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_metrics_is_zero_size() {
        assert_eq!(std::mem::size_of::<NoOpMetrics>(), 0);
    }
}
