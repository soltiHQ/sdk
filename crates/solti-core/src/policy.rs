use solti_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, RestartStrategy, Slot, TimeoutMs,
};

/// Runtime policy for a pre-built task.
///
/// This is similar to [`CreateSpec`] but does not carry [`solti_model::TaskKind`]:
/// the task body [`taskvisor::TaskRef`] is already constructed in code.
#[derive(Clone, Debug)]
pub struct TaskPolicy {
    pub slot: Slot,
    pub timeout_ms: TimeoutMs,
    pub restart: RestartStrategy,
    pub backoff: BackoffStrategy,
    pub admission: AdmissionStrategy,
}

impl TaskPolicy {
    /// Build a policy from a full `CreateSpec`, dropping the `kind` information.
    pub fn from_spec(spec: &CreateSpec) -> Self {
        Self {
            slot: spec.slot.clone(),
            timeout_ms: spec.timeout_ms,
            restart: spec.restart,
            backoff: spec.backoff.clone(),
            admission: spec.admission,
        }
    }

    /// Convenience constructor.
    pub fn new(
        slot: Slot,
        timeout_ms: TimeoutMs,
        restart: RestartStrategy,
        backoff: BackoffStrategy,
        admission: AdmissionStrategy,
    ) -> Self {
        Self {
            slot,
            timeout_ms,
            restart,
            backoff,
            admission,
        }
    }
}
