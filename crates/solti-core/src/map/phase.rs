//! Taskvisor to model phase crosswalk.
//!
//! This module is the single home for semantic mapping from Taskvisor's typed
//! final outcomes and rejection kinds into [`TaskPhase`]. Both state paths use
//! it:
//!
//! - the best-effort event path maps `TaskFinished.outcome_kind` and typed
//!   rejection events;
//! - the reliable completion path maps [`taskvisor::TaskOutcome`].
//!
//! Diagnostic `reason` text may be retained as an error message, but it never
//! selects a phase.
//!
//! ## Crosswalk
//!
//! | Taskvisor category                    | Model phase              | Why                                                        |
//! |----------------------------------------|--------------------------|------------------------------------------------------------|
//! | `TaskOutcomeKind::Completed`           | [`TaskPhase::Succeeded`] | The final attempt succeeded.                               |
//! | `TaskOutcomeKind::Failed`              | [`TaskPhase::Exhausted`] | A retryable failure reached its policy stop condition.     |
//! | `TaskOutcomeKind::Fatal`               | [`TaskPhase::Failed`]    | A permanent error stopped the task.                        |
//! | `TaskOutcomeKind::Canceled`            | [`TaskPhase::Canceled`]  | The task stopped cooperatively.                            |
//! | `TaskOutcomeKind::ForceAborted`        | [`TaskPhase::Canceled`]  | Cancellation completed through the runtime's abort path.   |
//! | `TaskOutcomeKind::Panicked`            | [`TaskPhase::Failed`]    | The internal managed runner failed.                        |
//! | cancel-like [`taskvisor::RejectionKind`] | [`TaskPhase::Canceled`] | Work was intentionally skipped or removed before running. |

use solti_model::TaskPhase;
use taskvisor::{RejectionKind, TaskOutcomeKind};

/// SDK-owned diagnostic used for a force-aborted final outcome.
///
/// The value preserves the SDK's existing status payload without depending on
/// Taskvisor's diagnostic `reason` strings.
pub(crate) const FORCE_ABORTED_ERROR: &str = "force_terminated_after_grace";

/// SDK-owned diagnostic used when Taskvisor reports an internal runner panic.
///
/// The value preserves the SDK's existing status payload.
pub(crate) const TASK_RUNNER_PANICKED_ERROR: &str = "actor panicked";

/// Classify a typed rejection into a terminal phase.
///
/// Applies to `ControllerRejected` / `TaskAddFailed` events and to
/// [`taskvisor::TaskOutcome::Rejected`]. User- or shutdown-initiated removals
/// and admission-policy skips are clean cancellation. Everything else is a
/// failed submission.
pub(crate) fn phase_for_rejection(kind: RejectionKind) -> TaskPhase {
    match kind {
        RejectionKind::SlotBusy
        | RejectionKind::RemovedFromQueue
        | RejectionKind::SupersededByReplace
        | RejectionKind::ControllerShuttingDown => TaskPhase::Canceled,
        _ => TaskPhase::Failed,
    }
}

/// Crosswalk a typed final outcome event into `(phase, error, exit_code)`.
///
/// `reason` is copied only for outcome categories that carry failure details.
/// It is never parsed or compared. Unknown future categories degrade to
/// [`TaskPhase::Failed`] with an SDK-owned diagnostic.
pub(crate) fn phase_for_outcome_kind(
    kind: TaskOutcomeKind,
    reason: Option<&str>,
    exit_code: Option<i32>,
) -> (TaskPhase, Option<String>, Option<i32>) {
    match kind {
        TaskOutcomeKind::Completed => (TaskPhase::Succeeded, None, None),
        TaskOutcomeKind::Failed => (
            TaskPhase::Exhausted,
            Some(
                reason
                    .unwrap_or("task retry policy stopped after a failure")
                    .to_string(),
            ),
            exit_code,
        ),
        TaskOutcomeKind::Fatal => (
            TaskPhase::Failed,
            Some(reason.unwrap_or("task reported a fatal error").to_string()),
            exit_code,
        ),
        TaskOutcomeKind::Canceled => (TaskPhase::Canceled, None, None),
        TaskOutcomeKind::ForceAborted => (
            TaskPhase::Canceled,
            Some(FORCE_ABORTED_ERROR.to_string()),
            None,
        ),
        TaskOutcomeKind::Panicked => (
            TaskPhase::Failed,
            Some(TASK_RUNNER_PANICKED_ERROR.to_string()),
            None,
        ),
        // Rejected work normally uses ControllerRejected / TaskAddFailed, not
        // TaskFinished. Without RejectionKind, failure is the conservative
        // event-side classification. The direct outcome path below retains the
        // precise typed rejection mapping.
        TaskOutcomeKind::Rejected => (
            TaskPhase::Failed,
            Some(reason.unwrap_or("task submission was rejected").to_string()),
            None,
        ),
        _ => (
            TaskPhase::Failed,
            Some("unknown task outcome kind".to_string()),
            exit_code,
        ),
    }
}

/// Crosswalk a direct [`taskvisor::TaskOutcome`] into
/// `(phase, error, exit_code)`.
///
/// Rejected work delegates to [`phase_for_rejection`]. All other known
/// variants share the same [`TaskOutcomeKind`] crosswalk as `TaskFinished`.
pub(crate) fn phase_for_outcome(
    outcome: &taskvisor::TaskOutcome,
) -> (TaskPhase, Option<String>, Option<i32>) {
    use taskvisor::TaskOutcome;

    match outcome {
        TaskOutcome::Failed {
            reason, exit_code, ..
        }
        | TaskOutcome::Fatal {
            reason, exit_code, ..
        } => phase_for_outcome_kind(outcome.kind(), Some(reason), *exit_code),
        TaskOutcome::Rejected { kind, reason, .. } => {
            (phase_for_rejection(*kind), Some(reason.to_string()), None)
        }
        _ => phase_for_outcome_kind(outcome.kind(), None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskvisor::TaskOutcome;

    #[test]
    fn rejection_cancel_like_kinds_map_to_canceled() {
        for kind in [
            RejectionKind::RemovedFromQueue,
            RejectionKind::SupersededByReplace,
            RejectionKind::ControllerShuttingDown,
            RejectionKind::SlotBusy,
        ] {
            assert_eq!(phase_for_rejection(kind), TaskPhase::Canceled);
        }
    }

    #[test]
    fn rejection_failure_kinds_map_to_failed() {
        for kind in [
            RejectionKind::QueueFull,
            RejectionKind::AlreadyExists,
            RejectionKind::BatchRejected,
            RejectionKind::AdmissionFailed,
        ] {
            assert_eq!(phase_for_rejection(kind), TaskPhase::Failed);
        }
    }

    #[test]
    fn typed_outcome_kinds_have_an_explicit_crosswalk() {
        for (kind, expected) in [
            (TaskOutcomeKind::Completed, TaskPhase::Succeeded),
            (TaskOutcomeKind::Failed, TaskPhase::Exhausted),
            (TaskOutcomeKind::Fatal, TaskPhase::Failed),
            (TaskOutcomeKind::Canceled, TaskPhase::Canceled),
            (TaskOutcomeKind::ForceAborted, TaskPhase::Canceled),
            (TaskOutcomeKind::Panicked, TaskPhase::Failed),
            (TaskOutcomeKind::Rejected, TaskPhase::Failed),
        ] {
            assert_eq!(
                phase_for_outcome_kind(kind, Some("diagnostic"), Some(9)).0,
                expected
            );
        }
    }

    #[test]
    fn diagnostic_text_never_selects_the_phase() {
        assert_eq!(
            phase_for_outcome_kind(
                TaskOutcomeKind::Completed,
                Some("retry limit reached after 3 retries"),
                Some(1),
            ),
            (TaskPhase::Succeeded, None, None),
        );
        assert_eq!(
            phase_for_outcome_kind(
                TaskOutcomeKind::Failed,
                Some("this text claims success"),
                Some(7),
            ),
            (
                TaskPhase::Exhausted,
                Some("this text claims success".to_string()),
                Some(7),
            ),
        );
    }

    #[test]
    fn direct_failed_and_fatal_outcomes_keep_diagnostics() {
        let failed = TaskOutcome::failed_for_tests("boom", Some(3));
        assert_eq!(
            phase_for_outcome(&failed),
            (TaskPhase::Exhausted, Some("boom".to_string()), Some(3)),
        );

        let fatal = TaskOutcome::fatal_for_tests("bad config", None);
        assert_eq!(
            phase_for_outcome(&fatal),
            (TaskPhase::Failed, Some("bad config".to_string()), None),
        );
    }

    #[test]
    fn direct_terminal_outcomes_share_the_typed_crosswalk() {
        assert_eq!(
            phase_for_outcome(&TaskOutcome::Completed),
            (TaskPhase::Succeeded, None, None),
        );
        assert_eq!(
            phase_for_outcome(&TaskOutcome::Canceled),
            (TaskPhase::Canceled, None, None),
        );
        assert_eq!(
            phase_for_outcome(&TaskOutcome::ForceAborted),
            (
                TaskPhase::Canceled,
                Some(FORCE_ABORTED_ERROR.to_string()),
                None,
            ),
        );
        assert_eq!(
            phase_for_outcome(&TaskOutcome::Panicked),
            (
                TaskPhase::Failed,
                Some(TASK_RUNNER_PANICKED_ERROR.to_string()),
                None,
            ),
        );
    }

    #[test]
    fn direct_rejection_uses_the_typed_rejection_classifier() {
        let canceled = TaskOutcome::rejected_for_tests(
            RejectionKind::RemovedFromQueue,
            "removed before registration",
        );
        assert_eq!(
            phase_for_outcome(&canceled),
            (
                TaskPhase::Canceled,
                Some("removed before registration".to_string()),
                None,
            ),
        );

        let failed = TaskOutcome::rejected_for_tests(
            RejectionKind::QueueFull,
            "slot queue reached capacity",
        );
        assert_eq!(phase_for_outcome(&failed).0, TaskPhase::Failed);
    }
}
