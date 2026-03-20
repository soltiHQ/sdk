//! Metrics for exec runner.

use solti_core::TaskOutcome;
use taskvisor::TaskError;

/// Subprocess runner type identifier for metrics.
pub const RUNNER_TYPE_SUBPROCESS: &str = "subprocess";

/// Wasm runner type identifier for metrics.
pub const RUNNER_TYPE_WASM: &str = "wasm";

/// Container runner type identifier for metrics.
pub const RUNNER_TYPE_CONTAINER: &str = "container";

/// Convert TaskError to TaskOutcome for metrics.
///
/// Note: `TaskError` is `#[non_exhaustive]`, so the catch-all arm is required.
/// All currently unknown future variants map to `Failure`.
pub fn task_error_to_outcome(error: &TaskError) -> TaskOutcome {
    match error {
        TaskError::Timeout { .. } => TaskOutcome::Timeout,
        TaskError::Canceled => TaskOutcome::Canceled,
        TaskError::Fail { .. } | TaskError::Fatal { .. } => TaskOutcome::Failure,
        _ => TaskOutcome::Failure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceled_maps_to_canceled() {
        let err = TaskError::Canceled;
        assert_eq!(task_error_to_outcome(&err), TaskOutcome::Canceled);
    }

    #[test]
    fn fail_maps_to_failure() {
        let err = TaskError::Fail {
            reason: "test".into(),
        };
        assert_eq!(task_error_to_outcome(&err), TaskOutcome::Failure);
    }

    #[test]
    fn fatal_maps_to_failure() {
        let err = TaskError::Fatal {
            reason: "test".into(),
        };
        assert_eq!(task_error_to_outcome(&err), TaskOutcome::Failure);
    }
}
