//! # Model ↔ taskvisor mapping.
//!
//! Adapter layer between `solti-model` (public specs) and the taskvisor runtime.
//!
//! Two directions, one module tree:
//! - this file maps model policies **into** taskvisor structures (submit path);
//! - [`phase`] maps taskvisor outcomes and reasons **back** into model phases
//!   (state-reconstruction path, shared by both planes).

pub(crate) mod phase;

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
        ModelJitterPolicy::Decorrelated => Ok(JitterPolicy::RandomizedBand),
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
    BackoffPolicy::new(
        Duration::from_millis(s.first_ms),
        Duration::from_millis(s.max_ms),
        s.factor,
        to_jitter_policy(s.jitter)?,
    )
    .map_err(|e| CoreError::Mapping(format!("invalid backoff policy: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_policy_maps_every_variant() {
        assert_eq!(
            to_admission_policy(ModelAdmissionPolicy::DropIfRunning).unwrap(),
            AdmissionPolicy::DropIfRunning
        );
        assert_eq!(
            to_admission_policy(ModelAdmissionPolicy::Replace).unwrap(),
            AdmissionPolicy::Replace
        );
        assert_eq!(
            to_admission_policy(ModelAdmissionPolicy::Queue).unwrap(),
            AdmissionPolicy::Queue
        );
    }

    #[test]
    fn jitter_policy_maps_every_variant() {
        assert_eq!(
            to_jitter_policy(ModelJitterPolicy::Decorrelated).unwrap(),
            JitterPolicy::RandomizedBand
        );
        assert_eq!(
            to_jitter_policy(ModelJitterPolicy::Equal).unwrap(),
            JitterPolicy::Equal
        );
        assert_eq!(
            to_jitter_policy(ModelJitterPolicy::Full).unwrap(),
            JitterPolicy::Full
        );
        assert_eq!(
            to_jitter_policy(ModelJitterPolicy::None).unwrap(),
            JitterPolicy::None
        );
    }

    #[test]
    fn restart_policy_maps_every_variant() {
        // taskvisor::RestartPolicy is not PartialEq; match structurally.
        assert!(matches!(
            to_restart_policy(ModelRestartPolicy::Never).unwrap(),
            RestartPolicy::Never
        ));
        assert!(matches!(
            to_restart_policy(ModelRestartPolicy::OnFailure).unwrap(),
            RestartPolicy::OnFailure
        ));
        assert!(matches!(
            to_restart_policy(ModelRestartPolicy::Always { interval_ms: None }).unwrap(),
            RestartPolicy::Always { interval: None }
        ));
        assert!(matches!(
            to_restart_policy(ModelRestartPolicy::Always {
                interval_ms: Some(2_500)
            })
            .unwrap(),
            RestartPolicy::Always { interval: Some(d) } if d == Duration::from_millis(2_500)
        ));
    }

    #[test]
    fn backoff_policy_maps_all_fields() {
        let model = ModelBackoffPolicy {
            jitter: ModelJitterPolicy::Equal,
            first_ms: 100,
            max_ms: 5_000,
            factor: 2.0,
        };
        let tv = to_backoff_policy(&model).unwrap();
        assert_eq!(tv.first(), Duration::from_millis(100));
        assert_eq!(tv.max(), Duration::from_millis(5_000));
        assert_eq!(tv.jitter(), JitterPolicy::Equal);
        assert_eq!(tv.factor(), 2.0);
    }
}
