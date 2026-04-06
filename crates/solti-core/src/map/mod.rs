//! Adapter layer between `solti-model` (public specs) and the taskvisor runtime.
//!
//! This crate maps high-level API types into taskvisor's internal execution structures.
use std::time::Duration;

use solti_model::{
    AdmissionPolicy as ModelAdmissionPolicy, BackoffPolicy as ModelBackoffPolicy,
    JitterPolicy as ModelJitterPolicy, RestartPolicy as ModelRestartPolicy,
};
use taskvisor::{AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy};

/// Convert a high-level admission policy from the public model into the controller admission policy used by taskvisor.
pub(crate) fn to_admission_policy(s: ModelAdmissionPolicy) -> AdmissionPolicy {
    match s {
        ModelAdmissionPolicy::DropIfRunning => AdmissionPolicy::DropIfRunning,
        ModelAdmissionPolicy::Replace => AdmissionPolicy::Replace,
        ModelAdmissionPolicy::Queue => AdmissionPolicy::Queue,
        _ => AdmissionPolicy::DropIfRunning,
    }
}

/// Convert a high-level jitter policy into the jitter policy used by taskvisor.
pub(crate) fn to_jitter_policy(s: ModelJitterPolicy) -> JitterPolicy {
    match s {
        ModelJitterPolicy::Decorrelated => JitterPolicy::Decorrelated,
        ModelJitterPolicy::Equal => JitterPolicy::Equal,
        ModelJitterPolicy::Full => JitterPolicy::Full,
        ModelJitterPolicy::None => JitterPolicy::None,
        _ => JitterPolicy::Full,
    }
}

/// Convert a high-level restart policy into the restart policy used by taskvisor.
pub(crate) fn to_restart_policy(s: ModelRestartPolicy) -> RestartPolicy {
    match s {
        ModelRestartPolicy::Always { interval_ms } => RestartPolicy::Always {
            interval: interval_ms.map(Duration::from_millis),
        },
        ModelRestartPolicy::OnFailure => RestartPolicy::OnFailure,
        ModelRestartPolicy::Never => RestartPolicy::Never,
        _ => RestartPolicy::Never,
    }
}

/// Convert a high-level backoff policy into a backoff policy used by taskvisor.
pub(crate) fn to_backoff_policy(s: &ModelBackoffPolicy) -> BackoffPolicy {
    BackoffPolicy {
        first: Duration::from_millis(s.first_ms),
        max: Duration::from_millis(s.max_ms),
        jitter: to_jitter_policy(s.jitter),
        factor: s.factor,
    }
}
