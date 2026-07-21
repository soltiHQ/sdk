//! Task condition conversion.

use solti_model::{ConditionStatus, TaskCondition};
use tracing::warn;

use super::time::system_time_to_ms;
use crate::proto_api;

impl From<ConditionStatus> for proto_api::ConditionStatus {
    fn from(status: ConditionStatus) -> Self {
        match status {
            ConditionStatus::Unknown => Self::Unknown,
            ConditionStatus::True => Self::True,
            ConditionStatus::False => Self::False,
            other => {
                warn!(
                    ?other,
                    "unknown ConditionStatus variant, mapping to Unspecified"
                );
                Self::Unspecified
            }
        }
    }
}

impl From<TaskCondition> for proto_api::TaskCondition {
    fn from(condition: TaskCondition) -> Self {
        Self {
            r#type: condition.condition_type.to_string(),
            status: proto_api::ConditionStatus::from(condition.status) as i32,
            observed_generation: condition.observed_generation,
            last_transition_time: system_time_to_ms(condition.last_transition_time),
            reason: condition.reason,
            message: condition.message,
        }
    }
}
