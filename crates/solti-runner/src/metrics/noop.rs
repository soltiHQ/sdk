//! # No-op metrics backend

use crate::metrics::backend::{MetricsBackend, RunnerErrorKind, RunnerType};

/// Zero-cost [`MetricsBackend`](super::MetricsBackend) that discards all records.
///
/// It is zero-sized and is the default backend of [`BuildContext`](crate::BuildContext).
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

    #[test]
    fn noop_can_be_called_repeatedly() {
        let metrics = NoOpMetrics;
        for _ in 0..1000 {
            metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
        }
    }
}
