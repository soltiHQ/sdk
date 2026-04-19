//! # `TaskPhase` ↔ `TaskStatus` conversion.
//!
//! Domain `TaskPhase` is `#[non_exhaustive]`.
//! The outgoing `From` impl logs and maps unknown variants to `Unspecified` instead of panicking.

use solti_model::TaskPhase;
use tracing::warn;

#[cfg(feature = "grpc")]
use crate::error::ApiError;
use crate::proto_api;

impl From<TaskPhase> for proto_api::TaskStatus {
    fn from(phase: TaskPhase) -> Self {
        match phase {
            TaskPhase::Succeeded => proto_api::TaskStatus::Succeeded,
            TaskPhase::Exhausted => proto_api::TaskStatus::Exhausted,
            TaskPhase::Canceled => proto_api::TaskStatus::Canceled,
            TaskPhase::Pending => proto_api::TaskStatus::Pending,
            TaskPhase::Running => proto_api::TaskStatus::Running,
            TaskPhase::Timeout => proto_api::TaskStatus::Timeout,
            TaskPhase::Failed => proto_api::TaskStatus::Failed,

            other => {
                warn!(?other, "unknown TaskPhase variant, mapping to Unspecified");
                proto_api::TaskStatus::Unspecified
            }
        }
    }
}

/// Convert a proto `TaskStatus` enum value (as `i32`) into domain [`TaskPhase`].
#[cfg(feature = "grpc")]
pub(crate) fn proto_to_domain_status(raw: i32) -> Result<TaskPhase, ApiError> {
    let status = proto_api::TaskStatus::try_from(raw)
        .map_err(|_| ApiError::InvalidRequest(format!("invalid status value: {raw}")))?;

    match status {
        proto_api::TaskStatus::Succeeded => Ok(TaskPhase::Succeeded),
        proto_api::TaskStatus::Exhausted => Ok(TaskPhase::Exhausted),
        proto_api::TaskStatus::Canceled => Ok(TaskPhase::Canceled),
        proto_api::TaskStatus::Pending => Ok(TaskPhase::Pending),
        proto_api::TaskStatus::Running => Ok(TaskPhase::Running),
        proto_api::TaskStatus::Timeout => Ok(TaskPhase::Timeout),
        proto_api::TaskStatus::Failed => Ok(TaskPhase::Failed),

        proto_api::TaskStatus::Unspecified => Err(ApiError::InvalidRequest(
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
            (TaskPhase::Pending, proto_api::TaskStatus::Pending),
            (TaskPhase::Running, proto_api::TaskStatus::Running),
            (TaskPhase::Succeeded, proto_api::TaskStatus::Succeeded),
            (TaskPhase::Failed, proto_api::TaskStatus::Failed),
            (TaskPhase::Timeout, proto_api::TaskStatus::Timeout),
            (TaskPhase::Canceled, proto_api::TaskStatus::Canceled),
            (TaskPhase::Exhausted, proto_api::TaskStatus::Exhausted),
        ];

        for (domain, expected_proto) in cases {
            let proto = proto_api::TaskStatus::from(domain);
            assert_eq!(proto, expected_proto, "mismatch for {:?}", domain);
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_status_rejects_unspecified() {
        let err = proto_to_domain_status(proto_api::TaskStatus::Unspecified as i32).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("unspecified")));
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_status_rejects_out_of_range() {
        let err = proto_to_domain_status(9999).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("9999")));
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn proto_to_domain_status_maps_known_variants() {
        for (raw, expected) in [
            (proto_api::TaskStatus::Pending, TaskPhase::Pending),
            (proto_api::TaskStatus::Running, TaskPhase::Running),
            (proto_api::TaskStatus::Succeeded, TaskPhase::Succeeded),
            (proto_api::TaskStatus::Failed, TaskPhase::Failed),
            (proto_api::TaskStatus::Timeout, TaskPhase::Timeout),
            (proto_api::TaskStatus::Canceled, TaskPhase::Canceled),
            (proto_api::TaskStatus::Exhausted, TaskPhase::Exhausted),
        ] {
            assert_eq!(proto_to_domain_status(raw as i32).unwrap(), expected);
        }
    }
}
