//! # No-op metrics backend.

use crate::metrics::backend::{MetricOutcome, MetricsBackend, RunnerErrorKind, RunnerType};

/// Zero-cost [`MetricsBackend`](super::MetricsBackend) that discards everything.
///
/// Zero-sized (`size_of::<NoOpMetrics>() == 0`) and the default backend of [`BuildContext`](crate::BuildContext);
/// every method is an `#[inline(always)]` empty body that compiles to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpMetrics;

impl MetricsBackend for NoOpMetrics {
    #[inline(always)]
    fn record_task_started(&self, _: RunnerType) {}

    #[inline(always)]
    fn record_task_completed(&self, _: RunnerType, _: MetricOutcome, _: u64) {}

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
            metrics.record_task_started(RunnerType::Subprocess);
            metrics.record_runner_error(RunnerType::Subprocess, RunnerErrorKind::SpawnFailed);
            metrics.record_task_completed(RunnerType::Subprocess, MetricOutcome::Success, 100);
        }
    }
}
