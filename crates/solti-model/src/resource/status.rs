//! # Task status
//!
//! [`TaskStatus`] is observed reconciliation and execution state.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{ConditionStatus, ModelError, ModelResult, TaskCondition, TaskPhase};

/// Observed runtime state of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "raw::TaskStatusRaw")]
pub struct TaskStatus {
    pub(crate) observed_generation: u64,
    pub(crate) phase: TaskPhase,
    pub(crate) attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    pub(crate) conditions: Vec<TaskCondition>,
}

impl TaskStatus {
    /// Creates an unobserved pending status.
    pub fn pending() -> Self {
        Self::pending_for(0, 0)
    }

    /// Reconstructs status from serialized fields.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when conditions are invalid or duplicated, or when the required `Reconciled` condition is missing.
    pub fn from_parts(
        observed_generation: u64,
        phase: TaskPhase,
        attempt: u32,
        exit_code: Option<i32>,
        error: Option<String>,
        conditions: Vec<TaskCondition>,
    ) -> ModelResult<Self> {
        let status = Self {
            observed_generation,
            phase,
            attempt,
            exit_code,
            error,
            conditions,
        };
        status.validate()?;
        Ok(status)
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

    /// Latest generation processed by the controller.
    pub fn observed_generation(&self) -> u64 {
        self.observed_generation
    }

    /// Current lifecycle phase.
    pub fn phase(&self) -> TaskPhase {
        self.phase
    }

    /// Current attempt number.
    ///
    /// Zero means no attempt has started.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Process exit code, when available.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Current lifecycle diagnostic, when available.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// All status conditions.
    pub fn conditions(&self) -> &[TaskCondition] {
        &self.conditions
    }

    /// Returns the serialized status fields.
    pub fn into_parts(
        self,
    ) -> (
        u64,
        TaskPhase,
        u32,
        Option<i32>,
        Option<String>,
        Vec<TaskCondition>,
    ) {
        (
            self.observed_generation,
            self.phase,
            self.attempt,
            self.exit_code,
            self.error,
            self.conditions,
        )
    }

    /// Returns a condition by type.
    pub fn condition(&self, condition_type: &crate::TaskConditionType) -> Option<&TaskCondition> {
        self.conditions
            .iter()
            .find(|condition| condition.condition_type() == condition_type)
    }

    /// Returns the required `Reconciled` condition.
    pub fn reconciled(&self) -> &TaskCondition {
        self.conditions
            .iter()
            .find(|condition| condition.condition_type().is_reconciled())
            .expect("validated TaskStatus has a Reconciled condition")
    }

    /// Returns whether reconciliation failed.
    pub fn reconciliation_failed(&self) -> bool {
        self.reconciled().status() == ConditionStatus::False
    }

    pub(crate) fn validate(&self) -> ModelResult<()> {
        let mut condition_types = HashSet::with_capacity(self.conditions.len());
        let mut reconciled_count = 0;
        for condition in &self.conditions {
            condition.validate()?;
            if !condition_types.insert(condition.condition_type().as_str()) {
                return Err(ModelError::Invalid(
                    format!(
                        "status.conditions contains duplicate type `{}`",
                        condition.condition_type()
                    )
                    .into(),
                ));
            }
            if condition.condition_type().is_reconciled() {
                reconciled_count += 1;
            }
        }
        if reconciled_count != 1 {
            return Err(ModelError::Invalid(
                "status.conditions must contain one Reconciled condition".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn reconciled_required(&self) -> &TaskCondition {
        self.reconciled()
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
            "runtime accepted the desired state",
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
            .find(|condition| condition.condition_type().is_reconciled())
            .expect("validated TaskStatus has a Reconciled condition")
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskStatusRaw {
        observed_generation: u64,
        phase: TaskPhase,
        attempt: u32,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        error: Option<String>,
        conditions: Vec<TaskCondition>,
    }

    impl TryFrom<TaskStatusRaw> for TaskStatus {
        type Error = ModelError;

        fn try_from(raw: TaskStatusRaw) -> Result<Self, Self::Error> {
            TaskStatus::from_parts(
                raw.observed_generation,
                raw.phase,
                raw.attempt,
                raw.exit_code,
                raw.error,
                raw.conditions,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskConditionType;
    use std::time::SystemTime;

    fn condition(condition_type: TaskConditionType, status: ConditionStatus) -> TaskCondition {
        TaskCondition::new(
            condition_type,
            status,
            1,
            SystemTime::UNIX_EPOCH,
            "Observed",
            "observed state",
        )
        .unwrap()
    }

    #[test]
    fn pending_is_valid() {
        let status = TaskStatus::pending();
        assert_eq!(status.phase(), TaskPhase::Pending);
        assert_eq!(status.attempt(), 0);
        assert!(status.error().is_none());
        assert_eq!(status.reconciled().status(), ConditionStatus::Unknown);
    }

    #[test]
    fn standalone_status_rejects_missing_reconciled_condition() {
        let json = serde_json::json!({
            "observedGeneration": 0,
            "phase": "pending",
            "attempt": 0,
            "conditions": []
        });
        assert!(serde_json::from_value::<TaskStatus>(json).is_err());
    }

    #[test]
    fn status_accepts_one_reconciled_and_extensible_conditions() {
        let reconciled = condition(TaskConditionType::reconciled(), ConditionStatus::True);
        let available_type = TaskConditionType::new("Available").unwrap();
        let available = condition(available_type.clone(), ConditionStatus::False);

        let status = TaskStatus::from_parts(
            1,
            TaskPhase::Running,
            1,
            None,
            None,
            vec![reconciled, available],
        )
        .unwrap();

        assert_eq!(status.conditions().len(), 2);
        assert_eq!(
            status.condition(&available_type).unwrap().status(),
            ConditionStatus::False
        );
        let back: TaskStatus =
            serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn status_rejects_duplicate_condition_types() {
        let reconciled = condition(TaskConditionType::reconciled(), ConditionStatus::True);
        let duplicate = reconciled.clone();

        assert!(
            TaskStatus::from_parts(
                1,
                TaskPhase::Running,
                1,
                None,
                None,
                vec![reconciled, duplicate],
            )
            .is_err()
        );
    }

    #[test]
    fn status_and_conditions_reject_unknown_fields() {
        let mut status = serde_json::to_value(TaskStatus::pending()).unwrap();
        status["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskStatus>(status).is_err());

        let mut status = serde_json::to_value(TaskStatus::pending()).unwrap();
        status["conditions"][0]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskStatus>(status).is_err());
    }
}
