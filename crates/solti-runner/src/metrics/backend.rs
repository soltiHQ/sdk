use std::sync::Arc;

/// Runner implementation type for metrics labeling.
///
/// Passed to [`MetricsBackend`] methods so dashboards can slice metrics
/// by runner backend (subprocess, wasm, container).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunnerType {
    /// OS subprocess runner.
    Subprocess,
    /// WebAssembly runner.
    Wasm,
    /// Container (OCI) runner.
    Container,
}

impl RunnerType {
    /// Return label value for metrics.
    #[inline]
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Subprocess => "subprocess",
            Self::Wasm => "wasm",
            Self::Container => "container",
        }
    }
}

/// Task execution outcome for metrics classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Task completed successfully.
    Success,
    /// Task failed.
    Failure,
    /// Task canceled.
    Canceled,
    /// Task timeout.
    Timeout,
}

impl TaskOutcome {
    /// Return label value for metrics.
    #[inline]
    pub fn as_label(&self) -> &'static str {
        match self {
            TaskOutcome::Success => "success",
            TaskOutcome::Failure => "failure",
            TaskOutcome::Canceled => "canceled",
            TaskOutcome::Timeout => "timeout",
        }
    }
}

/// Backend metrics collection interface.
///
/// This trait abstracts metrics collection across different backends.
/// Implementations are injected via [`crate::BuildContext`] and used by all runners.
pub trait MetricsBackend: Send + Sync + 'static {
    /// Record task spawn event.
    ///
    /// Called when a task is submitted and starts executing.
    fn record_task_started(&self, runner_type: RunnerType);

    /// Record task completion with outcome and duration.
    ///
    /// Called when task exits (success, failure, timeout, cancel).
    fn record_task_completed(
        &self,
        runner_type: RunnerType,
        outcome: TaskOutcome,
        duration_ms: u64,
    );

    /// Record runner-specific error during task setup/teardown.
    ///
    /// Called when runner fails to spawn/cleanup a task.
    /// This is separate from task failures (which are `record_task_completed` with `Failure`).
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: &str);
}

/// Shared handle to metrics backend.
///
/// Stored in [`crate::BuildContext`] and cloned into each task.
pub type MetricsHandle = Arc<dyn MetricsBackend>;
