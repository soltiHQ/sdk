use serde::{Deserialize, Serialize};

use crate::{
    LABEL_RUNNER_TAG, RunnerLabels,
    domain::{Slot, TimeoutMs},
    error::{ModelError, ModelResult},
    kind::TaskKind,
    strategy::{AdmissionStrategy, BackoffStrategy, RestartStrategy},
};

/// Declarative specification used when creating a new task.
///
/// `CreateSpec` describes *what* should be run and *how* it should be managed by the runtime.
///
/// Fields cover:
/// - logical grouping and concurrency control (`slot`, `admission`)
/// - execution backend (`kind`)
/// - lifecycle policies (`timeout_ms`, `restart`, `backoff`)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSpec {
    /// Logical slot name used for concurrency control.
    ///
    /// All tasks with the same slot share a single execution lane:
    /// admission rules decide what happens when a new task targets an already busy slot.
    pub slot: Slot,
    /// Execution backend used to run the task.
    ///
    /// This selects which runner is responsible (subprocess process, wasm, container, etc.).
    /// If no runner supports the given kind at runtime, task creation will fail.
    pub kind: TaskKind,
    /// Hard timeout for the task in milliseconds.
    ///
    /// Once this timeout is reached, the task is considered failed with timeout error.
    pub timeout_ms: TimeoutMs,
    /// Restart applied after a task completes or fails.
    ///
    /// Controls *whether* the task should be scheduled again (e.g. `OnFailure`, `Always`, `Never`).
    pub restart: RestartStrategy,
    /// Backoff configuration used between restart attempts.
    ///
    /// Defines *how long* to wait before the next run when the restart policy allows another attempt.
    pub backoff: BackoffStrategy,
    /// Admission for handling conflicts within the same slot.
    ///
    /// Controls what happens when a new task is submitted while a task in the same slot is already running (drop, replace, queue).
    pub admission: AdmissionStrategy,
    /// Optional metadata for routing / scheduling / observability.
    ///
    /// Router uses key `runner-tag` (if present) to select a specific runner among those that support this `TaskKind`.
    #[serde(default, skip_serializing_if = "RunnerLabels::is_empty")]
    pub labels: RunnerLabels,
}

impl CreateSpec {
    /// Attach a runner tag label used by the router.
    ///
    /// The tag is stored under the [`LABEL_RUNNER_TAG`] key and later
    /// consumed by `RunnerRouter` to pick a specific runner instance.
    ///
    /// This is a builder-style helper:
    ///
    /// ```rust
    /// # use solti_model::{
    /// #   CreateSpec, RunnerLabels, TaskKind, RestartStrategy, BackoffStrategy,
    /// #   AdmissionStrategy, JitterStrategy, TaskEnv, Flag,
    /// # };
    /// let spec = CreateSpec {
    ///     slot: "demo".into(),
    ///     kind: TaskKind::Subprocess {
    ///         command: "ls".into(),
    ///         args: vec!["/tmp".into()],
    ///         env: TaskEnv::default(),
    ///         cwd: None,
    ///         fail_on_non_zero: Flag::enabled(),
    ///     },
    ///     timeout_ms: 5_000_u64.into(),
    ///     restart: RestartStrategy::Never,
    ///     backoff: BackoffStrategy::default(),
    ///     admission: AdmissionStrategy::DropIfRunning,
    ///     labels: RunnerLabels::new(),
    /// }
    /// .with_runner_tag("runner-a");
    /// ```
    pub fn with_runner_tag(mut self, tag: impl Into<String>) -> Self {
        self.labels.insert(LABEL_RUNNER_TAG, tag);
        self
    }

    /// Return the runner tag label (if present).
    ///
    /// This is a thin wrapper over `labels.get(LABEL_RUNNER_TAG)` and is
    /// intended for consumers that perform routing / placement.
    pub fn runner_tag(&self) -> Option<&str> {
        self.labels.get(LABEL_RUNNER_TAG)
    }

    /// Validate the spec at the model level.
    ///
    /// Checks:
    /// - `slot` is not empty
    /// - `kind` is not [`TaskKind::None`] (internal-only, must use
    ///   `submit_with_task` instead)
    /// - `timeout_ms` is greater than zero
    ///
    /// Called automatically by `SupervisorApi::submit()`. Does **not** need
    /// to be called for specs built by internal tasks (timezone sync, discovery)
    /// which use `TaskKind::None` + `submit_with_task`.
    pub fn validate(&self) -> ModelResult<()> {
        if self.slot.as_str().is_empty() {
            return Err(ModelError::Invalid("slot must not be empty".into()));
        }
        if matches!(self.kind, TaskKind::None) {
            return Err(ModelError::Invalid(
                "TaskKind::None cannot be submitted via runner; use submit_with_task".into(),
            ));
        }
        if self.timeout_ms.as_millis() == 0 {
            return Err(ModelError::Invalid("timeout_ms must be greater than zero".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Flag, TaskEnv};

    fn valid_spec() -> CreateSpec {
        CreateSpec {
            slot: "test".into(),
            kind: TaskKind::Subprocess {
                command: "echo".into(),
                args: vec![],
                env: TaskEnv::default(),
                cwd: None,
                fail_on_non_zero: Flag::enabled(),
            },
            timeout_ms: 5_000_u64.into(),
            restart: RestartStrategy::Never,
            backoff: BackoffStrategy::default(),
            admission: AdmissionStrategy::DropIfRunning,
            labels: RunnerLabels::new(),
        }
    }

    #[test]
    fn valid_spec_passes() {
        assert!(valid_spec().validate().is_ok());
    }

    #[test]
    fn empty_slot_fails() {
        let mut spec = valid_spec();
        spec.slot = "".into();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("slot"));
    }

    #[test]
    fn kind_none_fails() {
        let mut spec = valid_spec();
        spec.kind = TaskKind::None;
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("TaskKind::None"));
    }

    #[test]
    fn zero_timeout_fails() {
        let mut spec = valid_spec();
        spec.timeout_ms = 0_u64.into();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("timeout_ms"));
    }
}
