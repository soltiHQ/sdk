use std::sync::Arc;

/// Task execution outcome for metrics classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    /// Task completed sucessfully.
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
    ///
    /// # Arguments
    /// - `runner_type`: Runner implementation
    fn record_task_started(&self, runner_type: &str);
    /// Record task completion with outcome and duration.
    ///
    /// Called when task exits (success, failure, timeout, cancel).
    ///
    /// # Arguments
    /// - `runner_type`: Runner implementation
    /// - `outcome`: How the task terminated
    /// - `duration_ms`: Execution time in milliseconds
    fn record_task_completed(&self, runner_type: &str, outcome: TaskOutcome, duration_ms: u64);
    /// Record runner-specific error during task setup/teardown.
    ///
    /// Called when runner fails to spawn/cleanup a task.
    /// This is separate from task failures (which are `record_task_completed` with `Failure`).
    ///
    /// # Arguments
    /// - `runner_type`: Runner implementation
    /// - `error_kind`: Error category
    fn record_runner_error(&self, runner_type: &str, error_kind: &str);
}

/// Shared handle to metrics backend.
///
/// Stored in [`crate::BuildContext`] and cloned into each task.
pub type MetricsHandle = Arc<dyn MetricsBackend>;
