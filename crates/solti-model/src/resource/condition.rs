//! # Task conditions
//!
//! [`TaskCondition`] records reconciliation state for one observed generation.

use std::{fmt, time::SystemTime};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ModelError, ModelResult, validation};

use super::metadata::time_serde;

pub(crate) const CONDITION_TYPE_MAX_BYTES: usize = 316;
pub(crate) const CONDITION_REASON_MAX_BYTES: usize = 1_024;
const CONDITION_MESSAGE_MAX_BYTES: usize = 32_768;

/// Stable and extensible type of condition reported for a [`Task`](crate::Task).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(
    feature = "schema",
    schemars(schema_with = "crate::schema::condition_type")
)]
pub struct TaskConditionType(String);

impl TaskConditionType {
    /// Creates a condition type.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is too long or violates qualified-name rules.
    pub fn new(value: impl Into<String>) -> ModelResult<Self> {
        let value = value.into();
        if value.len() > CONDITION_TYPE_MAX_BYTES {
            return Err(ModelError::Invalid(
                format!(
                    "condition type length {} exceeds max {CONDITION_TYPE_MAX_BYTES}",
                    value.len()
                )
                .into(),
            ));
        }
        validation::validate_qualified_name("condition type", &value)?;
        Ok(Self(value))
    }

    /// The controller condition for desired-state reconciliation.
    pub fn reconciled() -> Self {
        Self("Reconciled".into())
    }

    /// Returns the serialized condition type.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn is_reconciled(&self) -> bool {
        self.0 == "Reconciled"
    }
}

impl fmt::Display for TaskConditionType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for TaskConditionType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TaskConditionType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Condition status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, deny_unknown_fields))]
#[serde(rename_all = "camelCase", try_from = "raw::TaskConditionRaw")]
pub struct TaskCondition {
    #[serde(rename = "type")]
    condition_type: TaskConditionType,
    status: ConditionStatus,
    observed_generation: u64,
    #[serde(with = "super::metadata::rfc3339_time_serde")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::rfc3339_time")
    )]
    last_transition_time: SystemTime,
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::condition_reason")
    )]
    reason: String,
    #[cfg_attr(feature = "schema", schemars(length(max = 32_768)))]
    message: String,
}

impl TaskCondition {
    /// Creates a condition.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the reason or message is invalid.
    pub fn new(
        condition_type: TaskConditionType,
        status: ConditionStatus,
        observed_generation: u64,
        last_transition_time: SystemTime,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> ModelResult<Self> {
        let condition = Self {
            condition_type,
            status,
            observed_generation,
            last_transition_time,
            reason: reason.into(),
            message: message.into(),
        };
        condition.validate()?;
        Ok(condition)
    }

    pub(crate) fn reconciled_unknown(generation: u64) -> Self {
        Self::new(
            TaskConditionType::reconciled(),
            ConditionStatus::Unknown,
            generation,
            time_serde::now(),
            "ReconciliationScheduled",
            "runtime reconciliation is scheduled",
        )
        .expect("built-in Reconciled condition is valid")
    }

    /// Stable condition type.
    pub fn condition_type(&self) -> &TaskConditionType {
        &self.condition_type
    }

    /// Current three-valued condition status.
    pub fn status(&self) -> ConditionStatus {
        self.status
    }

    /// Desired generation described by this condition.
    pub fn observed_generation(&self) -> u64 {
        self.observed_generation
    }

    /// Time when `status` last changed.
    pub fn last_transition_time(&self) -> SystemTime {
        self.last_transition_time
    }

    /// Stable machine-readable reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Human-readable diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the serialized condition fields.
    pub fn into_parts(
        self,
    ) -> (
        TaskConditionType,
        ConditionStatus,
        u64,
        SystemTime,
        String,
        String,
    ) {
        (
            self.condition_type,
            self.status,
            self.observed_generation,
            self.last_transition_time,
            self.reason,
            self.message,
        )
    }

    pub(crate) fn validate_reason_message(reason: &str, message: &str) -> ModelResult<()> {
        let bytes = reason.as_bytes();
        if bytes.is_empty() {
            return Err(ModelError::Invalid(
                "condition reason must not be empty".into(),
            ));
        }
        if bytes.len() > CONDITION_REASON_MAX_BYTES {
            return Err(ModelError::Invalid(
                format!(
                    "condition reason length {} exceeds max {CONDITION_REASON_MAX_BYTES}",
                    bytes.len()
                )
                .into(),
            ));
        }
        let valid_reason = bytes[0].is_ascii_alphabetic()
            && (bytes[bytes.len() - 1].is_ascii_alphanumeric() || bytes[bytes.len() - 1] == b'_');
        let valid_reason = valid_reason
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b',' | b':'));
        if !valid_reason {
            return Err(ModelError::Invalid(
                "condition reason must follow Kubernetes reason rules".into(),
            ));
        }
        if message.len() > CONDITION_MESSAGE_MAX_BYTES {
            return Err(ModelError::Invalid(
                format!(
                    "condition message length {} exceeds max {CONDITION_MESSAGE_MAX_BYTES}",
                    message.len()
                )
                .into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> ModelResult<()> {
        Self::validate_reason_message(&self.reason, &self.message)
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

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskConditionRaw {
        #[serde(rename = "type")]
        condition_type: TaskConditionType,
        status: ConditionStatus,
        observed_generation: u64,
        #[serde(with = "super::super::metadata::rfc3339_time_serde")]
        last_transition_time: SystemTime,
        reason: String,
        message: String,
    }

    impl TryFrom<TaskConditionRaw> for TaskCondition {
        type Error = ModelError;

        fn try_from(raw: TaskConditionRaw) -> Result<Self, Self::Error> {
            TaskCondition::new(
                raw.condition_type,
                raw.status,
                raw.observed_generation,
                raw.last_transition_time,
                raw.reason,
                raw.message,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_type_is_extensible_and_validated() {
        let condition_type = TaskConditionType::new("example.io/Available").unwrap();
        assert_eq!(condition_type.as_str(), "example.io/Available");

        assert!(TaskConditionType::new("").is_err());
        assert!(TaskConditionType::new("bad type").is_err());
    }

    #[test]
    fn condition_rejects_invalid_kubernetes_reason() {
        let result = TaskCondition::new(
            TaskConditionType::reconciled(),
            ConditionStatus::False,
            1,
            SystemTime::UNIX_EPOCH,
            "invalid-reason",
            "diagnostic",
        );

        assert!(result.is_err());
    }
}
