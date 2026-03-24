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
#[serde(from = "raw::TaskSpecRaw")]
pub struct TaskSpec {
    slot: Slot,
    kind: TaskKind,

    timeout: Timeout,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    admission: AdmissionPolicy,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    runner_selector: Option<RunnerSelector>,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    labels: Labels,
}

impl TaskSpec {
    /// Logical slot name for concurrency control.
    #[inline]
    pub fn slot(&self) -> &Slot {
        &self.slot
    }

    /// Execution backend used to run the task.
    #[inline]
    pub fn kind(&self) -> &TaskKind {
        &self.kind
    }

    /// Hard timeout in milliseconds.
    #[inline]
    pub fn timeout(&self) -> Timeout {
        self.timeout
    }

    /// Restart policy applied after completion or failure.
    #[inline]
    pub fn restart(&self) -> RestartPolicy {
        self.restart
    }

    /// Backoff configuration between restart attempts.
    #[inline]
    pub fn backoff(&self) -> &BackoffPolicy {
        &self.backoff
    }

    /// Admission policy for handling slot conflicts.
    #[inline]
    pub fn admission(&self) -> AdmissionPolicy {
        self.admission
    }

    /// Label selector for runner routing (if present).
    #[inline]
    pub fn runner_selector(&self) -> Option<&RunnerSelector> {
        self.runner_selector.as_ref()
    }

    /// Metadata labels for routing / scheduling / observability.
    #[inline]
    pub fn labels(&self) -> &Labels {
        &self.labels
    }
}

impl TaskSpec {
    /// Create a [`TaskSpecBuilder`] with the three required fields.
    ///
    /// ```rust
    /// use solti_model::{TaskSpec, TaskKind, SubprocessSpec, SubprocessMode, RestartPolicy};
    ///
    /// let spec = TaskSpec::builder(
    ///     "my-slot",
    ///     TaskKind::Subprocess(SubprocessSpec {
    ///         mode: SubprocessMode::Command {
    ///             command: "echo".into(),
    ///             args: vec!["hello".into()],
    ///         },
    ///         env: Default::default(),
    ///         cwd: None,
    ///         fail_on_non_zero: Default::default(),
    ///     }),
    ///     5_000u64,
    /// )
    /// .restart(RestartPolicy::OnFailure)
    /// .build()
    /// .expect("valid spec");
    /// ```
    pub fn builder(
        slot: impl Into<Slot>,
        kind: TaskKind,
        timeout: impl Into<Timeout>,
    ) -> TaskSpecBuilder {
        TaskSpecBuilder::new(slot, kind, timeout)
    }
}

impl TaskSpec {
    /// Attach a runner selector used by the router (consuming builder-style).
    #[inline]
    pub fn with_runner_selector(mut self, sel: RunnerSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }
}

impl TaskSpec {
    /// Validate the spec at the **submit boundary**.
    ///
    /// Checks:
    /// - `slot` is not empty
    /// - `backoff` parameters are sane
    /// - `timeout` is greater than zero
    /// - `kind` specific constraints (e.g. non-empty command)
    /// - `runner_selector` requirements are structurally valid
    /// - `kind` is not [`TaskKind::Embedded`] (internal-only; use `submit_with_task` instead)
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
        self.kind.validate()?;
        self.backoff.validate()?;
        if let Some(ref sel) = self.runner_selector {
            for req in &sel.match_expressions {
                req.validate()?;
            }
        }
        Ok(())
    }

    /// Structural validation (everything except the [`TaskKind::Embedded`] business rule).
    ///
    /// Used by [`TaskSpecBuilder::build`].
    fn validate_structural(&self) -> ModelResult<()> {
        if self.slot.as_str().is_empty() {
            return Err(ModelError::Invalid("slot cannot be empty".into()));
        }
        if self.timeout.as_millis() == 0 {
            return Err(ModelError::Invalid(
                "timeout must be greater than zero".into(),
            ));
        }
        self.kind.validate()?;
        self.backoff.validate()?;
        if let Some(ref sel) = self.runner_selector {
            for req in &sel.match_expressions {
                req.validate()?;
            }
        }
        Ok(())
    }
}

/// Builder for [`TaskSpec`] that validates structural invariants on [`build`](TaskSpecBuilder::build).
///
/// Required fields (`slot`, `kind`, `timeout`) are set in the constructor.
/// Optional fields have sensible defaults:
/// - `backoff`: [`BackoffPolicy::default`] (full jitter, 1 s → 30 s, factor 2)
/// - `admission`: [`AdmissionPolicy::DropIfRunning`]
/// - `restart`: [`RestartPolicy::Never`]
/// - `runner_selector`: `None`
/// - `labels`: empty
pub struct TaskSpecBuilder {
    runner_selector: Option<RunnerSelector>,

    kind: TaskKind,
    slot: Slot,

    backoff: BackoffPolicy,
    restart: RestartPolicy,
    timeout: Timeout,

    admission: AdmissionPolicy,
    labels: Labels,
}

impl TaskSpecBuilder {
    fn new(slot: impl Into<Slot>, kind: TaskKind, timeout: impl Into<Timeout>) -> Self {
        Self {
            runner_selector: None,

            kind,
            slot: slot.into(),

            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            timeout: timeout.into(),

            admission: AdmissionPolicy::default(),
            labels: Labels::new(),
        }
    }

    /// Set restart policy.
    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Set backoff configuration.
    pub fn backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Set admission policy.
    pub fn admission(mut self, admission: AdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Set runner selector.
    pub fn runner_selector(mut self, sel: RunnerSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }

    /// Set metadata labels.
    pub fn labels(mut self, labels: Labels) -> Self {
        self.labels = labels;
        self
    }

    /// Build the [`TaskSpec`], validating structural invariants.
    ///
    /// This checks everything **except** the [`TaskKind::Embedded`] business rule
    /// (which is enforced at the submit boundary by [`TaskSpec::validate`]).
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] if:
    /// - `slot` is empty
    /// - `timeout` is zero
    /// - `kind` fails kind-specific validation
    /// - `backoff` parameters are invalid
    /// - `runner_selector` requirements are invalid
    pub fn build(self) -> ModelResult<TaskSpec> {
        let spec = TaskSpec {
            runner_selector: self.runner_selector,

            kind: self.kind,
            slot: self.slot,

            restart: self.restart,
            backoff: self.backoff,
            timeout: self.timeout,

            admission: self.admission,
            labels: self.labels,
        };
        spec.validate_structural()?;
        Ok(spec)
    }
}

/// Permissive deserialization (no validation).
/// Validation happens at the submit boundary via [`TaskSpec::validate`].
mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct TaskSpecRaw {
        slot: Slot,
        kind: TaskKind,
        timeout: Timeout,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        admission: AdmissionPolicy,

        #[serde(default)]
        labels: Labels,
        #[serde(default)]
        runner_selector: Option<RunnerSelector>,
    }

    impl From<TaskSpecRaw> for TaskSpec {
        fn from(r: TaskSpecRaw) -> Self {
            Self {
                runner_selector: r.runner_selector,

                kind: r.kind,
                slot: r.slot,

                restart: r.restart,
                backoff: r.backoff,
                timeout: r.timeout,

                admission: r.admission,
                labels: r.labels,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Flag, SubprocessMode, SubprocessSpec, TaskEnv};

    fn valid_spec() -> TaskSpec {
        TaskSpec::builder(
            "test",
            TaskKind::Subprocess(SubprocessSpec {
                mode: SubprocessMode::Command {
                    command: "echo".into(),
                    args: vec![],
                },
                env: TaskEnv::default(),
                cwd: None,
                fail_on_non_zero: Flag::enabled(),
            }),
            5_000u64,
        )
        .build()
        .expect("test spec must be valid")
    }

    #[test]
    fn valid_spec_passes() {
        assert!(valid_spec().validate().is_ok());
    }

    #[test]
    fn builder_rejects_empty_slot() {
        let err = TaskSpec::builder("", TaskKind::Embedded, 5_000u64)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("slot"));
    }

    #[test]
    fn builder_rejects_zero_timeout() {
        let err = TaskSpec::builder("test", TaskKind::Embedded, 0u64)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn builder_allows_embedded_kind() {
        let spec = TaskSpec::builder("test", TaskKind::Embedded, 5_000u64)
            .build()
            .expect("Embedded is structurally valid");
        assert!(matches!(spec.kind(), TaskKind::Embedded));
    }

    #[test]
    fn validate_rejects_embedded_kind() {
        let spec = TaskSpec::builder("test", TaskKind::Embedded, 5_000u64)
            .build()
            .unwrap();
        let err = spec.validate().unwrap_err();
        assert!(err.to_string().contains("TaskKind::Embedded"));
    }

    #[test]
    fn getters_return_expected_values() {
        let spec = TaskSpec::builder("my-slot", TaskKind::Embedded, 10_000u64)
            .restart(RestartPolicy::OnFailure)
            .admission(AdmissionPolicy::Replace)
            .build()
            .unwrap();

        assert_eq!(spec.slot(), "my-slot");
        assert_eq!(spec.timeout().as_millis(), 10_000);
        assert_eq!(spec.restart(), RestartPolicy::OnFailure);
        assert_eq!(spec.admission(), AdmissionPolicy::Replace);
    }

    #[test]
    fn serde_roundtrip() {
        let spec = valid_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: TaskSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}
