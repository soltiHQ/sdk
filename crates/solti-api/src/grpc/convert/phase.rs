//! # Phase Conversion
//!
//! Converts task phases in both directions.
//! Unknown domain variants have no v1 wire representation.
//! `Unspecified` and unknown wire values are invalid request input.

use solti_model::TaskPhase;

use crate::error::ApiError;
use crate::proto_api;

impl TryFrom<TaskPhase> for proto_api::TaskPhase {
    type Error = ApiError;

    fn try_from(phase: TaskPhase) -> Result<Self, Self::Error> {
        Ok(match phase {
            TaskPhase::Succeeded => Self::Succeeded,
            TaskPhase::Exhausted => Self::Exhausted,
            TaskPhase::Canceled => Self::Canceled,
            TaskPhase::Pending => Self::Pending,
            TaskPhase::Running => Self::Running,
            TaskPhase::Timeout => Self::Timeout,
            TaskPhase::Failed => Self::Failed,
            _ => {
                return Err(ApiError::Internal(
                    "handler returned an unsupported task phase".into(),
                ));
            }
        })
    }
}

/// Converts one raw protobuf phase into the domain enum.
#[cfg(feature = "grpc")]
pub(crate) fn proto_to_domain_phase(raw: i32) -> Result<TaskPhase, ApiError> {
    let status = proto_api::TaskPhase::try_from(raw)
        .map_err(|_| ApiError::InvalidRequest(format!("invalid status value: {raw}")))?;

    match status {
        proto_api::TaskPhase::Succeeded => Ok(TaskPhase::Succeeded),
        proto_api::TaskPhase::Exhausted => Ok(TaskPhase::Exhausted),
        proto_api::TaskPhase::Canceled => Ok(TaskPhase::Canceled),
        proto_api::TaskPhase::Pending => Ok(TaskPhase::Pending),
        proto_api::TaskPhase::Running => Ok(TaskPhase::Running),
        proto_api::TaskPhase::Timeout => Ok(TaskPhase::Timeout),
        proto_api::TaskPhase::Failed => Ok(TaskPhase::Failed),

        proto_api::TaskPhase::Unspecified => Err(ApiError::InvalidRequest(
            "status cannot be unspecified".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_phase_maps_all_known_variants_both_ways() {
        let cases = [
            (TaskPhase::Pending, proto_api::TaskPhase::Pending),
            (TaskPhase::Running, proto_api::TaskPhase::Running),
            (TaskPhase::Succeeded, proto_api::TaskPhase::Succeeded),
            (TaskPhase::Failed, proto_api::TaskPhase::Failed),
            (TaskPhase::Timeout, proto_api::TaskPhase::Timeout),
            (TaskPhase::Canceled, proto_api::TaskPhase::Canceled),
            (TaskPhase::Exhausted, proto_api::TaskPhase::Exhausted),
        ];

        for (domain, expected_proto) in cases {
            let proto = proto_api::TaskPhase::try_from(domain).unwrap();
            assert_eq!(proto, expected_proto, "mismatch for {:?}", domain);

            #[cfg(feature = "grpc")]
            assert_eq!(
                proto_to_domain_phase(expected_proto as i32).unwrap(),
                domain
            );
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_phase_rejects_unknown_values() {
        for (raw, expected_message) in [
            (
                proto_api::TaskPhase::Unspecified as i32,
                "unspecified".to_owned(),
            ),
            (9999, "9999".to_owned()),
        ] {
            let error = proto_to_domain_phase(raw).unwrap_err();
            assert!(
                matches!(error, ApiError::InvalidRequest(message) if message.contains(&expected_message))
            );
        }
    }
}
