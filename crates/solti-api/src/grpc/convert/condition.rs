//! # Condition Conversion
//!
//! Converts domain task conditions into protobuf response values.

use solti_model::{ConditionStatus, TaskCondition};

use super::time::system_time_to_ms;
use crate::error::ApiError;
use crate::proto_api;

impl TryFrom<ConditionStatus> for proto_api::ConditionStatus {
    type Error = ApiError;

    fn try_from(status: ConditionStatus) -> Result<Self, Self::Error> {
        Ok(match status {
            ConditionStatus::Unknown => Self::Unknown,
            ConditionStatus::True => Self::True,
            ConditionStatus::False => Self::False,
            _ => {
                return Err(ApiError::Internal(
                    "handler returned an unsupported condition status".into(),
                ));
            }
        })
    }
}

impl TryFrom<TaskCondition> for proto_api::TaskCondition {
    type Error = ApiError;

    fn try_from(condition: TaskCondition) -> Result<Self, Self::Error> {
        let (condition_type, status, observed_generation, transition_time, reason, message) =
            condition.into_parts();
        Ok(Self {
            r#type: condition_type.to_string(),
            status: proto_api::ConditionStatus::try_from(status)? as i32,
            observed_generation,
            last_transition_time: system_time_to_ms(transition_time)?,
            reason,
            message,
        })
    }
}
