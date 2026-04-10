use crate::metrics::backend::{MetricsBackend, RunnerType, TaskOutcome};

/// No-op metrics backend that compiles to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpMetrics;

impl MetricsBackend for NoOpMetrics {
    #[inline(always)]
    fn record_task_started(&self, _: RunnerType) {}

    #[inline(always)]
    fn record_task_completed(&self, _: RunnerType, _: TaskOutcome, _: u64) {}

    #[inline(always)]
    fn record_runner_error(&self, _: RunnerType, _: &str) {}
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
            metrics.record_runner_error(RunnerType::Subprocess, "error");
            metrics.record_task_completed(RunnerType::Subprocess, TaskOutcome::Success, 100);
        }
    }
}
