//! # Policy Conversion
//!
//! Converts restart, backoff, jitter, and admission policies.

use solti_model::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy};

use crate::{error::ApiError, proto_api};

pub(super) fn restart_to_proto(
    policy: RestartPolicy,
) -> Result<(proto_api::RestartPolicy, Option<u64>), ApiError> {
    Ok(match policy {
        RestartPolicy::Never => (proto_api::RestartPolicy::Never, None),
        RestartPolicy::OnFailure => (proto_api::RestartPolicy::OnFailure, None),
        RestartPolicy::Always { interval_ms } => (proto_api::RestartPolicy::Always, interval_ms),
        _ => {
            return Err(ApiError::Internal(
                "handler returned an unsupported restart policy".into(),
            ));
        }
    })
}

pub(super) fn backoff_to_proto(
    backoff: &BackoffPolicy,
) -> Result<proto_api::BackoffPolicy, ApiError> {
    let jitter = match backoff.jitter {
        JitterPolicy::None => proto_api::JitterPolicy::None,
        JitterPolicy::Full => proto_api::JitterPolicy::Full,
        JitterPolicy::Equal => proto_api::JitterPolicy::Equal,
        JitterPolicy::Decorrelated => proto_api::JitterPolicy::Decorrelated,
        _ => {
            return Err(ApiError::Internal(
                "handler returned an unsupported jitter policy".into(),
            ));
        }
    };
    Ok(proto_api::BackoffPolicy {
        jitter: jitter as i32,
        first_ms: backoff.first_ms,
        max_ms: backoff.max_ms,
        factor: backoff.factor,
    })
}

pub(super) fn admission_to_proto(
    policy: AdmissionPolicy,
) -> Result<proto_api::AdmissionPolicy, ApiError> {
    Ok(match policy {
        AdmissionPolicy::DropIfRunning => proto_api::AdmissionPolicy::DropIfRunning,
        AdmissionPolicy::Replace => proto_api::AdmissionPolicy::Replace,
        AdmissionPolicy::Queue => proto_api::AdmissionPolicy::Queue,
        _ => {
            return Err(ApiError::Internal(
                "handler returned an unsupported admission policy".into(),
            ));
        }
    })
}

pub(super) fn convert_restart_policy(
    strategy: proto_api::RestartPolicy,
    interval_ms: Option<u64>,
) -> Result<RestartPolicy, ApiError> {
    match strategy {
        proto_api::RestartPolicy::Never => Ok(RestartPolicy::Never),
        proto_api::RestartPolicy::OnFailure => Ok(RestartPolicy::OnFailure),
        proto_api::RestartPolicy::Always => Ok(RestartPolicy::Always { interval_ms }),
        proto_api::RestartPolicy::Unspecified => Err(ApiError::InvalidRequest(
            "restart strategy not specified".into(),
        )),
    }
}

pub(super) fn convert_backoff_policy(
    backoff: proto_api::BackoffPolicy,
) -> Result<BackoffPolicy, ApiError> {
    let jitter = proto_api::JitterPolicy::try_from(backoff.jitter)
        .map_err(|_| ApiError::InvalidRequest("invalid jitter strategy".into()))?;
    let jitter = match jitter {
        proto_api::JitterPolicy::Decorrelated => JitterPolicy::Decorrelated,
        proto_api::JitterPolicy::Equal => JitterPolicy::Equal,
        proto_api::JitterPolicy::None => JitterPolicy::None,
        proto_api::JitterPolicy::Full => JitterPolicy::Full,
        proto_api::JitterPolicy::Unspecified => {
            return Err(ApiError::InvalidRequest(
                "jitter strategy not specified".into(),
            ));
        }
    };
    Ok(BackoffPolicy {
        jitter,
        first_ms: backoff.first_ms,
        max_ms: backoff.max_ms,
        factor: backoff.factor,
    })
}

pub(super) fn convert_admission_policy(
    strategy: proto_api::AdmissionPolicy,
) -> Result<AdmissionPolicy, ApiError> {
    match strategy {
        proto_api::AdmissionPolicy::DropIfRunning => Ok(AdmissionPolicy::DropIfRunning),
        proto_api::AdmissionPolicy::Replace => Ok(AdmissionPolicy::Replace),
        proto_api::AdmissionPolicy::Queue => Ok(AdmissionPolicy::Queue),
        proto_api::AdmissionPolicy::Unspecified => Err(ApiError::InvalidRequest(
            "admission strategy not specified".into(),
        )),
    }
}
