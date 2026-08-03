//! # Runtime phase mapping
//!
//! This module maps typed Taskvisor results to [`TaskPhase`].
//! Both runtime state paths use the same crosswalk.
//!
//! ```text
//! TaskFinished event ──────┐
//!                          ├──► phase, error, exit code
//! direct TaskOutcome ──────┘
//! ```
//!
//! Diagnostic reason text may become an error message.
//! It never selects a phase.
//!
//! ## Crosswalk
//!
//! | Taskvisor category                             | Model phase              |
//! |------------------------------------------------|--------------------------|
//! | `TaskOutcomeKind::Completed`                   | [`TaskPhase::Succeeded`] |
//! | `TaskOutcomeKind::Failed`                      | [`TaskPhase::Exhausted`] |
//! | `TaskOutcomeKind::Fatal`                       | [`TaskPhase::Failed`]    |
//! | `TaskOutcomeKind::Canceled`                    | [`TaskPhase::Canceled`]  |
//! | `TaskOutcomeKind::ForceAborted`                | [`TaskPhase::Canceled`]  |
//! | `TaskOutcomeKind::Panicked`                    | [`TaskPhase::Failed`]    |
//! | `TaskOutcomeKind::Rejected` event              | [`TaskPhase::Failed`]    |
//! | Cancel-like [`taskvisor::RejectionKind`]       | [`TaskPhase::Canceled`]  |
//! | Other [`taskvisor::RejectionKind`]             | [`TaskPhase::Failed`]    |
//! | Unknown future outcome                         | [`TaskPhase::Failed`]    |

use solti_model::TaskPhase;
use taskvisor::{RejectionKind, TaskOutcomeKind};

/// Diagnostic for a force-aborted outcome.
///
/// The value does not depend on Taskvisor reason text.
pub(crate) const FORCE_ABORTED_ERROR: &str = "force_terminated_after_grace";

/// Diagnostic for an internal Taskvisor runner panic.
pub(crate) const TASK_RUNNER_PANICKED_ERROR: &str = "actor panicked";

/// Maps a typed rejection to a terminal phase.
///
/// Queue removal, replacement, shutdown, and a busy slot map to cancellation.
/// Other rejection kinds map to failure.
pub(crate) fn phase_for_rejection(kind: RejectionKind) -> TaskPhase {
    match kind {
        RejectionKind::SlotBusy
        | RejectionKind::RemovedFromQueue
        | RejectionKind::SupersededByReplace
        | RejectionKind::ControllerShuttingDown => TaskPhase::Canceled,
        _ => TaskPhase::Failed,
    }
}

/// Maps a typed final event to phase details.
///
/// Failure categories keep the supplied reason and exit code.
/// The reason is not parsed.
/// Unknown categories map to [`TaskPhase::Failed`].
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

/// Maps a direct [`taskvisor::TaskOutcome`] to phase details.
///
/// Rejected work uses [`phase_for_rejection`].
/// Other known outcomes use the event crosswalk.
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
    fn rejection_kinds_have_an_explicit_crosswalk() {
        for (kind, expected) in [
            (RejectionKind::RemovedFromQueue, TaskPhase::Canceled),
            (RejectionKind::SupersededByReplace, TaskPhase::Canceled),
            (RejectionKind::ControllerShuttingDown, TaskPhase::Canceled),
            (RejectionKind::SlotBusy, TaskPhase::Canceled),
            (RejectionKind::QueueFull, TaskPhase::Failed),
            (RejectionKind::AlreadyExists, TaskPhase::Failed),
            (RejectionKind::BatchRejected, TaskPhase::Failed),
            (RejectionKind::AdmissionFailed, TaskPhase::Failed),
        ] {
            assert_eq!(phase_for_rejection(kind), expected);
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
