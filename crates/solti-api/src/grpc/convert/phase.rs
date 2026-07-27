//! # `TaskPhase` conversion: domain enum ↔ wire enum.
//!
//! Domain `TaskPhase` is `#[non_exhaustive]`. Unknown domain variants are
//! rejected rather than emitted as `Unspecified`.

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

/// Convert a proto `TaskPhase` enum value (as `i32`) into domain [`TaskPhase`].
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
    fn task_phase_all_variants_round_trip() {
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
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_phase_rejects_unspecified() {
        let err = proto_to_domain_phase(proto_api::TaskPhase::Unspecified as i32).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("unspecified")));
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_phase_rejects_out_of_range() {
        let err = proto_to_domain_phase(9999).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("9999")));
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_phase_maps_known_variants() {
        for (raw, expected) in [
            (proto_api::TaskPhase::Pending, TaskPhase::Pending),
            (proto_api::TaskPhase::Running, TaskPhase::Running),
            (proto_api::TaskPhase::Succeeded, TaskPhase::Succeeded),
            (proto_api::TaskPhase::Failed, TaskPhase::Failed),
            (proto_api::TaskPhase::Timeout, TaskPhase::Timeout),
            (proto_api::TaskPhase::Canceled, TaskPhase::Canceled),
            (proto_api::TaskPhase::Exhausted, TaskPhase::Exhausted),
        ] {
            assert_eq!(proto_to_domain_phase(raw as i32).unwrap(), expected);
        }
    }
}
