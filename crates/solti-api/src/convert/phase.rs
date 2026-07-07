//! # `TaskPhase` conversion: domain enum ↔ wire enum.
//!
//! Domain `TaskPhase` is `#[non_exhaustive]`.
//! The outgoing `From` impl logs and maps unknown variants to `Unspecified` instead of panicking.

use solti_model::TaskPhase;
use tracing::warn;

#[cfg(feature = "grpc")]
use crate::error::ApiError;
use crate::proto_api;

impl From<TaskPhase> for proto_api::TaskPhase {
    fn from(phase: TaskPhase) -> Self {
        match phase {
            TaskPhase::Succeeded => proto_api::TaskPhase::Succeeded,
            TaskPhase::Exhausted => proto_api::TaskPhase::Exhausted,
            TaskPhase::Canceled => proto_api::TaskPhase::Canceled,
            TaskPhase::Pending => proto_api::TaskPhase::Pending,
            TaskPhase::Running => proto_api::TaskPhase::Running,
            TaskPhase::Timeout => proto_api::TaskPhase::Timeout,
            TaskPhase::Failed => proto_api::TaskPhase::Failed,

            other => {
                warn!(?other, "unknown TaskPhase variant, mapping to Unspecified");
                proto_api::TaskPhase::Unspecified
            }
        }
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
            let proto = proto_api::TaskPhase::from(domain);
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
