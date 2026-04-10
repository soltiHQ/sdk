//! # Policy mapping.
//!
//! Adapter layer between `solti-model` (public specs) and the taskvisor runtime.
//! Maps high-level API types into taskvisor's internal execution structures.
//!
//! All model enums are `#[non_exhaustive]` unknown variants produce [`CoreError::Mapping`](crate::CoreError::Mapping) instead of a silent fallback.

use std::time::Duration;

use solti_model::{
    AdmissionPolicy as ModelAdmissionPolicy, BackoffPolicy as ModelBackoffPolicy,
    JitterPolicy as ModelJitterPolicy, RestartPolicy as ModelRestartPolicy,
};
use taskvisor::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy};

use crate::error::CoreError;

/// Convert a high-level admission policy from the public model into the controller admission policy used by taskvisor.
pub(crate) fn to_admission_policy(s: ModelAdmissionPolicy) -> Result<AdmissionPolicy, CoreError> {
    match s {
        ModelAdmissionPolicy::DropIfRunning => Ok(AdmissionPolicy::DropIfRunning),
        ModelAdmissionPolicy::Replace => Ok(AdmissionPolicy::Replace),
        ModelAdmissionPolicy::Queue => Ok(AdmissionPolicy::Queue),
        other => Err(CoreError::Mapping(format!(
            "unknown AdmissionPolicy variant: {other:?}"
        ))),
    }
}

/// Convert a high-level jitter policy into the jitter policy used by taskvisor.
pub(crate) fn to_jitter_policy(s: ModelJitterPolicy) -> Result<JitterPolicy, CoreError> {
    match s {
        ModelJitterPolicy::Decorrelated => Ok(JitterPolicy::Decorrelated),
        ModelJitterPolicy::Equal => Ok(JitterPolicy::Equal),
        ModelJitterPolicy::Full => Ok(JitterPolicy::Full),
        ModelJitterPolicy::None => Ok(JitterPolicy::None),
        other => Err(CoreError::Mapping(format!(
            "unknown JitterPolicy variant: {other:?}"
        ))),
    }
}

/// Convert a high-level restart policy into the restart policy used by taskvisor.
pub(crate) fn to_restart_policy(s: ModelRestartPolicy) -> Result<RestartPolicy, CoreError> {
    match s {
        ModelRestartPolicy::Always { interval_ms } => Ok(RestartPolicy::Always {
            interval: interval_ms.map(Duration::from_millis),
        }),
        ModelRestartPolicy::OnFailure => Ok(RestartPolicy::OnFailure),
        ModelRestartPolicy::Never => Ok(RestartPolicy::Never),
        other => Err(CoreError::Mapping(format!(
            "unknown RestartPolicy variant: {other:?}"
        ))),
    }
}

/// Convert a high-level backoff policy into a backoff policy used by taskvisor.
pub(crate) fn to_backoff_policy(s: &ModelBackoffPolicy) -> Result<BackoffPolicy, CoreError> {
    Ok(BackoffPolicy {
        first: Duration::from_millis(s.first_ms),
        max: Duration::from_millis(s.max_ms),
        jitter: to_jitter_policy(s.jitter)?,
        factor: s.factor,
    })
}
