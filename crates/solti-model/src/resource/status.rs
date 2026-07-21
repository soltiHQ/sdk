//! Task status.
//!
//! [`TaskStatus`] keeps controller reconciliation conditions separate from the
//! execution phase, attempt, exit code, and execution error.

use serde::{Deserialize, Serialize};

use crate::{ConditionStatus, TaskCondition, TaskConditionType, TaskPhase};

/// Observed runtime state of a task.
///
/// ## Example
///
/// ```
/// use solti_model::{TaskPhase, TaskStatus};
///
/// let status = TaskStatus::pending();
/// assert_eq!(status.phase, TaskPhase::Pending);
/// assert_eq!(status.attempt, 0);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    /// Latest desired-state generation processed by the controller.
    pub observed_generation: u64,
    /// Current lifecycle phase.
    pub phase: TaskPhase,
    /// Number of execution attempts.
    pub attempt: u32,
    /// Process exit code (Subprocess/Container only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last lifecycle diagnostic, when one accompanies the current phase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Orthogonal controller conditions for the current desired generation.
    pub conditions: Vec<TaskCondition>,
}

impl TaskStatus {
    /// Create initial pending status.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{TaskPhase, TaskStatus};
    ///
    /// let status = TaskStatus::pending();
    /// assert_eq!(status.phase, TaskPhase::Pending);
    /// assert!(status.error.is_none());
    /// ```
    pub fn pending() -> Self {
        Self::pending_for(0, 0)
    }

    pub(crate) fn pending_for(observed_generation: u64, desired_generation: u64) -> Self {
        Self {
            observed_generation,
            phase: TaskPhase::Pending,
            exit_code: None,
            error: None,
            attempt: 0,
            conditions: vec![TaskCondition::reconciled_unknown(desired_generation)],
        }
    }

    pub(crate) fn pending_after(&self, desired_generation: u64) -> Self {
        let mut pending = Self {
            observed_generation: self.observed_generation,
            phase: TaskPhase::Pending,
            exit_code: None,
            error: None,
            attempt: 0,
            conditions: self.conditions.clone(),
        };
        pending.mark_reconciliation_pending(desired_generation);
        pending
    }

    /// Return the controller's `Reconciled` condition.
    pub fn reconciled(&self) -> &TaskCondition {
        self.conditions
            .iter()
            .find(|condition| condition.condition_type == TaskConditionType::Reconciled)
            .expect("validated TaskStatus always has a Reconciled condition")
    }

    /// Return whether the current generation ended in a reconciliation failure.
    pub fn reconciliation_failed(&self) -> bool {
        self.reconciled().status == ConditionStatus::False
    }

    pub(crate) fn mark_reconciliation_pending(&mut self, generation: u64) -> bool {
        self.reconciled_mut().transition(
            ConditionStatus::Unknown,
            generation,
            "ReconciliationScheduled",
            "runtime reconciliation is scheduled",
        )
    }

    pub(crate) fn mark_reconciled(&mut self, generation: u64) -> bool {
        let changed = self.reconciled_mut().transition(
            ConditionStatus::True,
            generation,
            "RuntimeAccepted",
            "Taskvisor accepted the runtime realization",
        );
        self.observed_generation = generation;
        changed
    }

    pub(crate) fn mark_reconciliation_failed(
        &mut self,
        generation: u64,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> bool {
        let changed =
            self.reconciled_mut()
                .transition(ConditionStatus::False, generation, reason, message);
        self.observed_generation = generation;
        changed
    }

    fn reconciled_mut(&mut self) -> &mut TaskCondition {
        self.conditions
            .iter_mut()
            .find(|condition| condition.condition_type == TaskConditionType::Reconciled)
            .expect("validated TaskStatus always has a Reconciled condition")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_default() {
        let s = TaskStatus::pending();
        assert_eq!(s.phase, TaskPhase::Pending);
        assert_eq!(s.attempt, 0);
        assert!(s.error.is_none());
    }

    #[test]
    fn error_stored() {
        let s = TaskStatus {
            observed_generation: 2,
            phase: TaskPhase::Failed,
            attempt: 3,
            exit_code: None,
            error: Some("timeout".into()),
            conditions: vec![TaskCondition::reconciled_unknown(2)],
        };
        assert_eq!(s.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn serde_skips_none_error() {
        let s = TaskStatus::pending();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn serde_roundtrip() {
        let s = TaskStatus {
            observed_generation: 2,
            phase: TaskPhase::Running,
            attempt: 2,
            exit_code: None,
            error: None,
            conditions: vec![TaskCondition::reconciled_unknown(2)],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase, TaskPhase::Running);
        assert_eq!(back.attempt, 2);
    }

    #[test]
    fn exit_code_serde() {
        let s = TaskStatus {
            observed_generation: 1,
            phase: TaskPhase::Failed,
            attempt: 1,
            exit_code: Some(137),
            error: Some("killed".into()),
            conditions: vec![TaskCondition::reconciled_unknown(1)],
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"exitCode\":137"));

        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exit_code, Some(137));
    }

    #[test]
    fn serde_skips_none_exit_code() {
        let s = TaskStatus::pending();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("exitCode"));
    }

    #[test]
    fn reconciliation_condition_tracks_controller_state_separately_from_phase() {
        let mut status = TaskStatus::pending_for(0, 1);
        assert_eq!(status.reconciled().status, ConditionStatus::Unknown);
        assert!(status.mark_reconciliation_failed(1, "RunnerBuildFailed", "runner unavailable"));
        assert_eq!(status.phase, TaskPhase::Pending);
        assert_eq!(status.reconciled().status, ConditionStatus::False);
        assert!(status.reconciliation_failed());

        assert!(status.mark_reconciliation_pending(1));
        assert_eq!(status.reconciled().status, ConditionStatus::Unknown);
        assert!(status.mark_reconciled(1));
        assert_eq!(status.reconciled().status, ConditionStatus::True);
        assert_eq!(status.observed_generation, 1);
    }
}
