//! # taskvisor → model phase crosswalk.
//!
//! The single home for the semantic mapping from taskvisor vocabulary
//! (outcomes, event `reason` strings) into [`TaskPhase`].
//!
//! Both state-reconstruction planes call these classifiers:
//!
//! - the **event plane** ([`StateSubscriber`](crate::state::StateSubscriber))
//!   for `ActorExhausted` and rejection events;
//! - the **completion plane** (`SupervisorApi::finalize_from_outcome`)
//!   for the guaranteed [`taskvisor::TaskOutcome`].
//!
//! One implementation guarantees the planes never disagree on a final phase.
//!
//! ## The subtle rows
//!
//! | taskvisor says | model phase | Why |
//! |----------------|-------------|-----|
//! | `TaskOutcome::Failed` | [`TaskPhase::Exhausted`] | The retry budget ran out; the task kept failing. |
//! | `TaskOutcome::Fatal` | [`TaskPhase::Failed`] | A permanent error stopped the task at once. |
//! | rejection with a cancel-like reason | [`TaskPhase::Canceled`] | User- or shutdown-initiated, not an error. |

use solti_model::TaskPhase;

use taskvisor::reasons;

/// Classify a rejection reason into a terminal phase.
///
/// Applies to `ControllerRejected` / `TaskAddFailed` events and to
/// [`taskvisor::TaskOutcome::Rejected`]. User- or shutdown-initiated removals
/// ([`reasons::REMOVED_FROM_QUEUE`], [`reasons::SUPERSEDED_BY_REPLACE`],
/// [`reasons::CONTROLLER_SHUTTING_DOWN`]) are a clean [`TaskPhase::Canceled`].
/// Everything else (queue full, duplicate name, ...) is [`TaskPhase::Failed`].
pub(crate) fn phase_for_rejection(reason: &str) -> TaskPhase {
    match reason {
        reasons::REMOVED_FROM_QUEUE
        | reasons::SUPERSEDED_BY_REPLACE
        | reasons::CONTROLLER_SHUTTING_DOWN => TaskPhase::Canceled,
        _ => TaskPhase::Failed,
    }
}

/// Classify an `ActorExhausted` reason into `(phase, error)`.
///
/// - [`reasons::POLICY_EXHAUSTED_SUCCESS`]: a normal one-shot completion → [`TaskPhase::Succeeded`], no error.
/// - [`reasons::TASK_RETURNED_CANCELED`]: a cooperative self-stop → [`TaskPhase::Canceled`], no error.
/// - Anything else (`max_retries_exceeded(...)`, ...) → [`TaskPhase::Exhausted`] with the reason as the error text.
pub(crate) fn phase_for_exhausted(reason: Option<&str>) -> (TaskPhase, Option<String>) {
    match reason {
        Some(reasons::POLICY_EXHAUSTED_SUCCESS) => (TaskPhase::Succeeded, None),
        Some(reasons::TASK_RETURNED_CANCELED) => (TaskPhase::Canceled, None),
        Some(other) => (TaskPhase::Exhausted, Some(other.to_string())),
        None => (TaskPhase::Exhausted, Some("exhausted".to_string())),
    }
}

/// Crosswalk a guaranteed [`taskvisor::TaskOutcome`] into `(phase, error, exit_code)`.
///
/// `Rejected` delegates to [`phase_for_rejection`], keeping both planes on one
/// classifier. Unknown future variants degrade to [`TaskPhase::Failed`] with a
/// diagnostic error text.
pub(crate) fn phase_for_outcome(
    outcome: &taskvisor::TaskOutcome,
) -> (TaskPhase, Option<String>, Option<i32>) {
    use taskvisor::TaskOutcome;

    match outcome {
        TaskOutcome::Completed => (TaskPhase::Succeeded, None, None),
        TaskOutcome::Failed {
            reason, exit_code, ..
        } => (TaskPhase::Exhausted, Some(reason.to_string()), *exit_code),
        TaskOutcome::Fatal {
            reason, exit_code, ..
        } => (TaskPhase::Failed, Some(reason.to_string()), *exit_code),
        TaskOutcome::Canceled => (TaskPhase::Canceled, None, None),
        TaskOutcome::ForceAborted => (
            TaskPhase::Canceled,
            Some("force_terminated_after_grace".to_string()),
            None,
        ),
        TaskOutcome::Panicked => (TaskPhase::Failed, Some("actor panicked".to_string()), None),
        TaskOutcome::Rejected { reason } => {
            (phase_for_rejection(reason), Some(reason.to_string()), None)
        }
        _ => (
            TaskPhase::Failed,
            Some("unknown task outcome".to_string()),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use taskvisor::TaskOutcome;

    #[test]
    fn rejection_cancel_like_reasons_map_to_canceled() {
        for reason in [
            reasons::REMOVED_FROM_QUEUE,
            reasons::SUPERSEDED_BY_REPLACE,
            reasons::CONTROLLER_SHUTTING_DOWN,
        ] {
            assert_eq!(phase_for_rejection(reason), TaskPhase::Canceled);
        }
    }

    #[test]
    fn rejection_other_reasons_map_to_failed() {
        for reason in ["queue_full: 3/3", "already_exists", ""] {
            assert_eq!(phase_for_rejection(reason), TaskPhase::Failed);
        }
    }

    #[test]
    fn exhausted_success_and_self_cancel_carry_no_error() {
        assert_eq!(
            phase_for_exhausted(Some(reasons::POLICY_EXHAUSTED_SUCCESS)),
            (TaskPhase::Succeeded, None)
        );
        assert_eq!(
            phase_for_exhausted(Some(reasons::TASK_RETURNED_CANCELED)),
            (TaskPhase::Canceled, None)
        );
    }

    #[test]
    fn exhausted_retry_limit_keeps_the_reason_as_error() {
        let (phase, error) = phase_for_exhausted(Some("max_retries_exceeded(3/3): boom"));
        assert_eq!(phase, TaskPhase::Exhausted);
        assert_eq!(error.as_deref(), Some("max_retries_exceeded(3/3): boom"));

        let (phase, error) = phase_for_exhausted(None);
        assert_eq!(phase, TaskPhase::Exhausted);
        assert_eq!(error.as_deref(), Some("exhausted"));
    }

    #[test]
    fn outcome_failed_is_exhausted_and_fatal_is_failed() {
        // The two subtle rows of the crosswalk (see the module doc table).
        let failed = TaskOutcome::failed_for_tests("boom", Some(3));
        let (phase, error, exit_code) = phase_for_outcome(&failed);
        assert_eq!(phase, TaskPhase::Exhausted);
        assert_eq!(error.as_deref(), Some("boom"));
        assert_eq!(exit_code, Some(3));

        let fatal = TaskOutcome::fatal_for_tests("bad config", None);
        let (phase, error, exit_code) = phase_for_outcome(&fatal);
        assert_eq!(phase, TaskPhase::Failed);
        assert_eq!(error.as_deref(), Some("bad config"));
        assert_eq!(exit_code, None);
    }

    #[test]
    fn outcome_terminal_variants_map_cleanly() {
        let (phase, error, _) = phase_for_outcome(&TaskOutcome::Panicked);
        assert_eq!(phase, TaskPhase::Failed);
        assert_eq!(error.as_deref(), Some("actor panicked"));

        assert_eq!(
            phase_for_outcome(&TaskOutcome::Completed),
            (TaskPhase::Succeeded, None, None)
        );
        assert_eq!(
            phase_for_outcome(&TaskOutcome::Canceled),
            (TaskPhase::Canceled, None, None)
        );
        let (phase, error, _) = phase_for_outcome(&TaskOutcome::ForceAborted);
        assert_eq!(phase, TaskPhase::Canceled);
        assert_eq!(error.as_deref(), Some("force_terminated_after_grace"));
    }

    #[test]
    fn outcome_rejected_uses_the_shared_rejection_classifier() {
        let canceled = TaskOutcome::Rejected {
            reason: reasons::REMOVED_FROM_QUEUE.into(),
        };
        let (phase, error, _) = phase_for_outcome(&canceled);
        assert_eq!(phase, TaskPhase::Canceled);
        assert_eq!(error.as_deref(), Some(reasons::REMOVED_FROM_QUEUE));

        let failed = TaskOutcome::Rejected {
            reason: "queue_full: 3/3".into(),
        };
        let (phase, _, _) = phase_for_outcome(&failed);
        assert_eq!(phase, TaskPhase::Failed);
    }
}
