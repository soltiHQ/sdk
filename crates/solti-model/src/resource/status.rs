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
    /// Creates an unobserved pending status for a desired generation.
    ///
    /// `observedGeneration` starts at zero.
    /// The `Reconciled` condition refers to `desired_generation`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `desired_generation` is zero.
    pub fn pending(desired_generation: u64) -> ModelResult<Self> {
        if desired_generation == 0 {
            return Err(ModelError::Invalid(
                "desired generation must be greater than zero".into(),
            ));
        }
        Ok(Self {
            observed_generation: 0,
            phase: TaskPhase::Pending,
            exit_code: None,
            error: None,
            attempt: 0,
            conditions: vec![TaskCondition::reconciled_unknown(desired_generation)],
        })
    }

    /// Reconstructs status from serialized fields.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when status fields are inconsistent.
    /// This includes lifecycle fields, conditions, and generations.
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
        let reconciled = self.reconciled();
        if reconciled.observed_generation() == 0 {
            return Err(ModelError::Invalid(
                "status.conditions[type=Reconciled].observedGeneration must be greater than zero"
                    .into(),
            ));
        }
        if reconciled.observed_generation() < self.observed_generation {
            return Err(ModelError::Invalid(
                "status.conditions[type=Reconciled].observedGeneration cannot be less than status.observedGeneration"
                    .into(),
            ));
        }
        if reconciled.status() != ConditionStatus::Unknown
            && reconciled.observed_generation() != self.observed_generation
        {
            return Err(ModelError::Invalid(
                "status.conditions[type=Reconciled].observedGeneration must equal status.observedGeneration when Reconciled is True or False"
                    .into(),
            ));
        }
        if self.phase != TaskPhase::Pending && reconciled.status() != ConditionStatus::True {
            return Err(ModelError::Invalid(
                "status.phase requires a Reconciled=True condition unless phase is pending".into(),
            ));
        }
        Self::validate_execution_fields(
            self.phase,
            self.attempt,
            self.exit_code,
            self.error.as_deref(),
        )?;
        Ok(())
    }

    fn validate_execution_fields(
        phase: TaskPhase,
        attempt: u32,
        exit_code: Option<i32>,
        error: Option<&str>,
    ) -> ModelResult<()> {
        match phase {
            TaskPhase::Pending => {
                if attempt != 0 {
                    return Err(ModelError::Invalid(
                        "status.attempt must be zero while status.phase is pending".into(),
                    ));
                }
                if exit_code.is_some() || error.is_some() {
                    return Err(ModelError::Invalid(
                        "status.exitCode and status.error must be absent while status.phase is pending"
                            .into(),
                    ));
                }
            }
            TaskPhase::Running => {
                if attempt == 0 {
                    return Err(ModelError::Invalid(
                        "status.attempt must be greater than zero while status.phase is running"
                            .into(),
                    ));
                }
                if exit_code.is_some() || error.is_some() {
                    return Err(ModelError::Invalid(
                        "status.exitCode and status.error must be absent while status.phase is running"
                            .into(),
                    ));
                }
            }
            TaskPhase::Succeeded
            | TaskPhase::Failed
            | TaskPhase::Timeout
            | TaskPhase::Canceled
            | TaskPhase::Exhausted => {}
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
    fn pending_generation_is_explicit() {
        let status = TaskStatus::pending(3).unwrap();
        assert_eq!(status.phase(), TaskPhase::Pending);
        assert_eq!(status.observed_generation(), 0);
        assert_eq!(status.attempt(), 0);
        assert!(status.error().is_none());
        assert_eq!(status.reconciled().status(), ConditionStatus::Unknown);
        assert_eq!(status.reconciled().observed_generation(), 3);
        assert!(TaskStatus::pending(0).is_err());
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
    fn status_rejects_inconsistent_phase_fields() {
        let cases = [
            (TaskPhase::Pending, 1, None, None),
            (TaskPhase::Pending, 0, Some(0), None),
            (TaskPhase::Pending, 0, None, Some("error".into())),
            (TaskPhase::Running, 0, None, None),
            (TaskPhase::Running, 1, Some(0), None),
            (TaskPhase::Running, 1, None, Some("error".into())),
        ];
        for (phase, attempt, exit_code, error) in cases {
            assert!(
                TaskStatus::from_parts(
                    1,
                    phase,
                    attempt,
                    exit_code,
                    error,
                    vec![condition(
                        TaskConditionType::reconciled(),
                        ConditionStatus::True
                    )],
                )
                .is_err()
            );
        }
    }

    #[test]
    fn status_enforces_reconciled_generation_contract() {
        let reconciled = |status, observed_generation| {
            TaskCondition::new(
                TaskConditionType::reconciled(),
                status,
                observed_generation,
                SystemTime::UNIX_EPOCH,
                "Observed",
                "observed state",
            )
            .unwrap()
        };

        assert!(
            TaskStatus::from_parts(
                1,
                TaskPhase::Pending,
                0,
                None,
                None,
                vec![reconciled(ConditionStatus::Unknown, 2)],
            )
            .is_ok()
        );
        for (observed_generation, phase, condition_status, condition_generation) in [
            (0, TaskPhase::Pending, ConditionStatus::Unknown, 0),
            (2, TaskPhase::Pending, ConditionStatus::Unknown, 1),
            (1, TaskPhase::Pending, ConditionStatus::True, 2),
            (1, TaskPhase::Running, ConditionStatus::Unknown, 1),
            (1, TaskPhase::Failed, ConditionStatus::False, 1),
        ] {
            assert!(
                TaskStatus::from_parts(
                    observed_generation,
                    phase,
                    if phase == TaskPhase::Running { 1 } else { 0 },
                    None,
                    None,
                    vec![reconciled(condition_status, condition_generation)],
                )
                .is_err()
            );
        }
    }

    #[test]
    fn terminal_status_allows_an_unknown_attempt() {
        let status = TaskStatus::from_parts(
            1,
            TaskPhase::Failed,
            0,
            None,
            Some("submission failed before an attempt started".into()),
            vec![condition(
                TaskConditionType::reconciled(),
                ConditionStatus::True,
            )],
        )
        .unwrap();

        assert_eq!(status.attempt(), 0);
    }

    #[test]
    fn status_and_conditions_reject_unknown_fields() {
        let mut status = serde_json::to_value(TaskStatus::pending(1).unwrap()).unwrap();
        status["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskStatus>(status).is_err());

        let mut status = serde_json::to_value(TaskStatus::pending(1).unwrap()).unwrap();
        status["conditions"][0]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskStatus>(status).is_err());
    }
}
