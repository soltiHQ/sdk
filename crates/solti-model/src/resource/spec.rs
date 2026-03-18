use serde::{Deserialize, Serialize};

use crate::{
    AdmissionPolicy, BackoffPolicy, Labels, RestartPolicy, RunnerSelector, Slot, TaskKind, Timeout,
    error::{ModelError, ModelResult},
};

/// Desired state specification.
///
/// `TaskSpec` describes *what* should be run and *how* it should be managed by the runtime.
///
/// Fields cover:
/// - logical grouping (`slot`)
/// - execution backend (`kind`)
/// - concurrency control (`admission`)
/// - lifecycle policies (`timeout`, `restart`, `backoff`)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSpec {
    /// Logical slot name for concurrency control.
    ///
    /// A slot groups tasks that share a single execution lane.
    /// The controller enforces admission policies per slot.
    pub slot: Slot,
    /// Execution backend used to run the task.
    ///
    /// This selects which runner is responsible (subprocess process, wasm, container, etc.).
    pub kind: TaskKind,
    /// Hard timeout for the task in milliseconds.
    ///
    /// Once this timeout is reached, the task is considered failed with timeout error.
    pub timeout: Timeout,
    /// Restart applied after a task completes or fails.
    ///
    /// Controls *whether* the task should be scheduled again (e.g. `OnFailure`, `Always`, `Never`).
    pub restart: RestartPolicy,
    /// Backoff configuration used between restart attempts.
    ///
    /// Defines *how long* to wait before the next run when the restart policy allows another attempt.
    pub backoff: BackoffPolicy,
    /// Admission for handling conflicts within the same slot.
    ///
    /// Controls what happens when a new task is submitted while a task in the same slot is already running (drop, replace, queue).
    pub admission: AdmissionPolicy,
    /// Label selector for runner routing.
    ///
    /// If set, the [`RunnerRouter`] will only pick runners whose labels satisfy all `match_labels` and `match_expressions` requirements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_selector: Option<RunnerSelector>,
    /// Optional metadata for routing / scheduling / observability.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
}

impl TaskSpec {
    /// Attach a runner selector used by the router.
    ///
    /// ```rust
    /// # use solti_model::{
    /// #   TaskSpec, Labels, TaskKind, RestartPolicy, BackoffPolicy,
    /// #   AdmissionPolicy, JitterPolicy, TaskEnv, Flag, RunnerSelector,
    /// # };
    /// # use std::collections::BTreeMap;
    /// let spec = TaskSpec {
    ///     slot: "demo".into(),
    ///     kind: TaskKind::Subprocess {
    ///         command: "ls".into(),
    ///         args: vec!["/tmp".into()],
    ///         env: TaskEnv::default(),
    ///         cwd: None,
    ///         fail_on_non_zero: Flag::enabled(),
    ///     },
    ///     timeout: 5_000_u64.into(),
    ///     restart: RestartPolicy::Never,
    ///     backoff: BackoffPolicy::default(),
    ///     admission: AdmissionPolicy::DropIfRunning,
    ///     runner_selector: None,
    ///     labels: Labels::new(),
    /// }
    /// .with_runner_selector(RunnerSelector::from_labels(BTreeMap::from([
    ///     ("runner-name".into(), "runner-a".into()),
    /// ])));
    /// ```
    #[inline]
    pub fn with_runner_selector(mut self, sel: RunnerSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }

    /// Return the runner selector (if present).
    #[inline]
    pub fn runner_selector(&self) -> Option<&RunnerSelector> {
        self.runner_selector.as_ref()
    }

    /// Validate the spec at the model level.
    ///
    /// Checks:
    /// - `slot` is not empty
    /// - `timeout` is greater than zero
    /// - `kind` is not [`TaskKind::Embedded`] (internal-only, must use `submit_with_task` instead)
    ///
    /// Called automatically by `SupervisorApi::submit()`.
    /// Does **not** need to be called for specs built by internal tasks
    /// (timezone sync, discovery) which use `TaskKind::Embedded` + `submit_with_task`.
    pub fn validate(&self) -> ModelResult<()> {
        if self.slot.as_str().is_empty() {
            return Err(ModelError::Invalid("slot cannot be empty".into()));
        }
        if matches!(self.kind, TaskKind::Embedded) {
            return Err(ModelError::Invalid(
                "TaskKind::Embedded cannot be submitted via runner; use submit_with_task".into(),
            ));
        }
        if self.timeout.as_millis() == 0 {
            return Err(ModelError::Invalid(
                "timeout must be greater than zero".into(),
            ));
        }
        self.backoff.validate()?;
        if let Some(ref sel) = self.runner_selector {
            for req in &sel.match_expressions {
                req.validate()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Flag, TaskEnv};

    fn valid_spec() -> TaskSpec {
        TaskSpec {
            slot: "test".into(),
            kind: TaskKind::Subprocess {
                command: "echo".into(),
                args: vec![],
                env: TaskEnv::default(),
                cwd: None,
                fail_on_non_zero: Flag::enabled(),
            },
            timeout: 5_000_u64.into(),
            restart: RestartPolicy::Never,
            backoff: BackoffPolicy::default(),
            admission: AdmissionPolicy::DropIfRunning,
            runner_selector: None,
            labels: Labels::new(),
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
    fn kind_embedded_fails() {
        let mut spec = valid_spec();
        spec.kind = TaskKind::Embedded;
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("TaskKind::Embedded"));
    }

    #[test]
    fn zero_timeout_fails() {
        let mut spec = valid_spec();
        spec.timeout = 0_u64.into();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }
}
