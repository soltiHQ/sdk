//! Adapter layer between `solti-model` (public specs) and the taskvisor runtime.
//!
//! This crate maps high-level API types into taskvisor’s internal execution structures.
use std::time::Duration;

use solti_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy,
};
use taskvisor::{
    AdmissionPolicy, BackoffPolicy, ControllerSpec, JitterPolicy, RestartPolicy, TaskRef, TaskSpec,
};

/// Convert a high-level admission strategy from the public model into the controller admission policy used by taskvisor.
pub fn to_admission_policy(s: AdmissionStrategy) -> AdmissionPolicy {
    match s {
        AdmissionStrategy::DropIfRunning => AdmissionPolicy::DropIfRunning,
        AdmissionStrategy::Replace => AdmissionPolicy::Replace,
        AdmissionStrategy::Queue => AdmissionPolicy::Queue,
    }
}

/// Convert a high-level jitter strategy into the jitter policy used by taskvisor.
pub fn to_jitter_policy(s: JitterStrategy) -> JitterPolicy {
    match s {
        JitterStrategy::Decorrelated => JitterPolicy::Decorrelated,
        JitterStrategy::Equal => JitterPolicy::Equal,
        JitterStrategy::Full => JitterPolicy::Full,
        JitterStrategy::None => JitterPolicy::None,
    }
}

/// Convert a high-level restart strategy into the restart policy used by taskvisor.
pub fn to_restart_policy(s: RestartStrategy) -> RestartPolicy {
    match s {
        RestartStrategy::Always { interval_ms } => RestartPolicy::Always {
            interval: interval_ms.map(Duration::from_millis),
        },
        RestartStrategy::OnFailure => RestartPolicy::OnFailure,
        RestartStrategy::Never => RestartPolicy::Never,
    }
}

/// Convert a high-level backoff strategy into a backoff policy used by taskvisor.
pub fn to_backoff_policy(s: &BackoffStrategy) -> BackoffPolicy {
    BackoffPolicy {
        first: Duration::from_millis(s.first_ms),
        max: Duration::from_millis(s.max_ms),
        jitter: to_jitter_policy(s.jitter),
        factor: s.factor,
    }
}

/// Build a `TaskSpec` from a public `CreateSpec`.
pub fn to_task_spec(task: TaskRef, s: &CreateSpec) -> TaskSpec {
    TaskSpec::new(
        task,
        to_restart_policy(s.restart),
        to_backoff_policy(&s.backoff),
        Some(Duration::from_millis(s.timeout_ms)),
    )
}

/// Build a `ControllerSpec` from a public `CreateSpec`.
pub fn to_controller_spec(task: TaskRef, s: &CreateSpec) -> ControllerSpec {
    ControllerSpec {
        admission: to_admission_policy(s.admission),
        task_spec: to_task_spec(task, s),
    }
}
