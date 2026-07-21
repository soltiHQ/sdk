//! Kubernetes-shaped task status conditions.

use std::{fmt, time::SystemTime};

use serde::{Deserialize, Serialize};

use super::metadata::time_serde;

/// Stable type of a condition reported for a [`Task`](crate::Task).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub enum TaskConditionType {
    /// Whether the desired generation was accepted by the runtime controller.
    Reconciled,
}

impl fmt::Display for TaskConditionType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reconciled => formatter.write_str("Reconciled"),
        }
    }
}

/// Kubernetes-compatible three-valued condition status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ConditionStatus {
    /// The controller has not yet determined the outcome.
    Unknown,
    /// The condition currently holds.
    True,
    /// The condition currently does not hold.
    False,
}

/// One observed condition for a Task resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskCondition {
    /// Stable condition type.
    #[serde(rename = "type")]
    pub condition_type: TaskConditionType,
    /// Current three-valued condition status.
    pub status: ConditionStatus,
    /// Desired generation this condition describes.
    pub observed_generation: u64,
    /// Time when `status` last changed.
    #[serde(with = "super::metadata::rfc3339_time_serde")]
    pub last_transition_time: SystemTime,
    /// Stable machine-readable reason.
    pub reason: String,
    /// Human-readable diagnostic.
    pub message: String,
}

impl TaskCondition {
    pub(crate) fn reconciled_unknown(generation: u64) -> Self {
        Self {
            condition_type: TaskConditionType::Reconciled,
            status: ConditionStatus::Unknown,
            observed_generation: generation,
            last_transition_time: time_serde::now(),
            reason: "ReconciliationScheduled".into(),
            message: "runtime reconciliation is scheduled".into(),
        }
    }

    pub(crate) fn transition(
        &mut self,
        status: ConditionStatus,
        generation: u64,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> bool {
        let reason = reason.into();
        let message = message.into();
        let changed = self.status != status
            || self.observed_generation != generation
            || self.reason != reason
            || self.message != message;
        if !changed {
            return false;
        }
        if self.status != status {
            self.last_transition_time = time_serde::now();
        }
        self.status = status;
        self.observed_generation = generation;
        self.reason = reason;
        self.message = message;
        true
    }
}
