//! Task specification.
//!
//! [`TaskSpec`] defines desired state: what to run and how the supervisor should manage it.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{
    AdmissionPolicy, BackoffPolicy, Labels, RestartPolicy, RunnerSelector, Slot, TaskKind, Timeout,
    error::{ModelError, ModelResult},
};

/// Desired state for a task.
///
/// Build it with [`TaskSpec::builder`]. Fields are private so every spec goes through validation.
///
/// ## Example
///
/// ```
/// use solti_model::{RestartPolicy, TaskKind, TaskSpec};
///
/// let spec = TaskSpec::builder("daily-cleanup", TaskKind::Embedded, 5_000u64)
///     .restart(RestartPolicy::periodic(60_000))
///     .build()
///     .unwrap();
///
/// assert_eq!(spec.slot().as_str(), "daily-cleanup");
/// assert_eq!(spec.timeout().as_millis(), 5_000);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "raw::TaskSpecRaw")]
pub struct TaskSpec {
    slot: Slot,
    kind: TaskKind,

    timeout: Timeout,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    admission: AdmissionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_retries: Option<NonZeroU32>,

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

    /// Maximum failure-driven retries per run.
    ///
    /// `None` means unlimited, the default.
    /// The invariant lives in the type: a zero budget is not representable.
    /// Counts only failure retries (the counter resets on success); when the budget is exhausted the supervisor stops restarting the task.
    #[inline]
    pub fn max_retries(&self) -> Option<NonZeroU32> {
        self.max_retries
    }

    /// Label selector for runner routing, if present.
    #[inline]
    pub fn runner_selector(&self) -> Option<&RunnerSelector> {
        self.runner_selector.as_ref()
    }

    /// Metadata labels for routing, scheduling, and observability.
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
    ///     TaskKind::Subprocess(SubprocessSpec::new(
    ///         SubprocessMode::Command {
    ///             command: "echo".into(),
    ///             args: vec!["hello".into()],
    ///         },
    ///         Default::default(),
    ///         None,
    ///         Default::default(),
    ///     )),
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
    /// Attach a runner selector used by the router.
    ///
    /// This is useful when a spec came from a stored value and a caller wants to add routing before submit.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{Labels, RunnerSelector, TaskKind, TaskSpec};
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("zone", "eu");
    ///
    /// let spec = TaskSpec::builder("build", TaskKind::Embedded, 1_000u64)
    ///     .build()
    ///     .unwrap()
    ///     .with_runner_selector(RunnerSelector::from_labels(labels));
    ///
    /// assert!(spec.runner_selector().is_some());
    /// ```
    #[inline]
    pub fn with_runner_selector(mut self, sel: RunnerSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }

    /// Override the admission policy.
    ///
    /// Used by apply or upgrade paths that need to force [`AdmissionPolicy::Replace`] regardless of the original spec.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{AdmissionPolicy, TaskKind, TaskSpec};
    ///
    /// let spec = TaskSpec::builder("agent", TaskKind::Embedded, 1_000u64)
    ///     .build()
    ///     .unwrap()
    ///     .with_admission(AdmissionPolicy::Replace);
    ///
    /// assert_eq!(spec.admission(), AdmissionPolicy::Replace);
    /// ```
    #[inline]
    pub fn with_admission(mut self, admission: AdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }
}

impl TaskSpec {
    /// Validate the spec at the submit boundary.
    ///
    /// Runs the full structural validation and then rejects [`TaskKind::Embedded`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{
    ///     Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec,
    /// };
    ///
    /// let spec = TaskSpec::builder(
    ///     "hello",
    ///     TaskKind::Subprocess(SubprocessSpec::new(
    ///         SubprocessMode::Command {
    ///             command: "echo".into(),
    ///             args: vec!["hello".into()],
    ///         },
    ///         TaskEnv::default(),
    ///         None,
    ///         Flag::enabled(),
    ///     )),
    ///     1_000u64,
    /// )
    /// .build()
    /// .unwrap();
    ///
    /// spec.validate().unwrap();
    /// ```
    pub fn validate(&self) -> ModelResult<()> {
        self.validate_structural()?;
        if matches!(self.kind, TaskKind::Embedded) {
            return Err(ModelError::Invalid(
                "TaskKind::Embedded cannot be submitted via runner; use submit_with_task".into(),
            ));
        }
        Ok(())
    }

    /// Structural validation of all fields.
    ///
    /// Checks:
    /// - `slot` is not empty
    /// - `backoff` parameters are sane
    /// - `timeout` is greater than zero
    /// - `kind` specific constraints (e.g. non-empty command)
    /// - `runner_selector` requirements are structurally valid
    fn validate_structural(&self) -> ModelResult<()> {
        self.slot.validate_format()?;
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

/// Builder for [`TaskSpec`].
///
/// Required fields (`slot`, `kind`, `timeout`) are set in the constructor.
/// Optional fields have sensible defaults:
/// - `backoff`: [`BackoffPolicy::default`] (full jitter, 1 second to 30 seconds, factor 2)
/// - `admission`: [`AdmissionPolicy::DropIfRunning`]
/// - `restart`: [`RestartPolicy::Never`]
/// - `runner_selector`: `None`
/// - `labels`: empty
///
/// ## Example
///
/// ```
/// use solti_model::{AdmissionPolicy, RestartPolicy, TaskKind, TaskSpec};
///
/// let spec = TaskSpec::builder("service", TaskKind::Embedded, 5_000u64)
///     .restart(RestartPolicy::always())
///     .admission(AdmissionPolicy::Replace)
///     .build()
///     .unwrap();
///
/// assert_eq!(spec.restart(), RestartPolicy::always());
/// assert_eq!(spec.admission(), AdmissionPolicy::Replace);
/// ```
pub struct TaskSpecBuilder {
    runner_selector: Option<RunnerSelector>,

    kind: TaskKind,
    slot: Slot,

    backoff: BackoffPolicy,
    restart: RestartPolicy,
    timeout: Timeout,
    max_retries: Option<NonZeroU32>,

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
            max_retries: None,
            labels: Labels::new(),
        }
    }

    /// Set restart policy.
    #[must_use]
    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Set the failure-retry budget. `None` means unlimited, the default.
    ///
    /// Mirrors taskvisor's signature: pass `NonZeroU32::new(n)` directly.
    ///
    /// ```rust
    /// # use solti_model::{TaskSpec, TaskKind};
    /// # use std::num::NonZeroU32;
    /// let spec = TaskSpec::builder("s", TaskKind::Embedded, 1_000u64)
    ///     .max_retries(NonZeroU32::new(3))
    ///     .build()
    ///     .expect("valid spec");
    /// assert_eq!(spec.max_retries().map(NonZeroU32::get), Some(3));
    /// ```
    #[must_use]
    pub fn max_retries(mut self, max_retries: impl Into<Option<NonZeroU32>>) -> Self {
        self.max_retries = max_retries.into();
        self
    }

    /// Set backoff configuration.
    #[must_use]
    pub fn backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Set admission policy.
    #[must_use]
    pub fn admission(mut self, admission: AdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Set runner selector.
    #[must_use]
    pub fn runner_selector(mut self, sel: RunnerSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }

    /// Set metadata labels.
    #[must_use]
    pub fn labels(mut self, labels: Labels) -> Self {
        self.labels = labels;
        self
    }

    /// Build the [`TaskSpec`], validating structural invariants.
    ///
    /// This checks everything **except** the [`TaskKind::Embedded`] business rule
    /// (which is enforced at the submit boundary by [`TaskSpec::validate`]).
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{TaskKind, TaskSpec};
    ///
    /// let err = TaskSpec::builder("", TaskKind::Embedded, 1_000u64)
    ///     .build()
    ///     .unwrap_err();
    ///
    /// assert!(err.to_string().contains("slot"));
    /// ```
    pub fn build(self) -> ModelResult<TaskSpec> {
        let spec = TaskSpec {
            runner_selector: self.runner_selector,

            kind: self.kind,
            slot: self.slot,

            restart: self.restart,
            backoff: self.backoff,
            timeout: self.timeout,

            admission: self.admission,
            max_retries: self.max_retries,
            labels: self.labels,
        };
        spec.validate_structural()?;
        Ok(spec)
    }
}

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
        max_retries: Option<u32>,

        #[serde(default)]
        labels: Labels,
        #[serde(default)]
        runner_selector: Option<RunnerSelector>,
    }

    impl TryFrom<TaskSpecRaw> for TaskSpec {
        type Error = ModelError;

        fn try_from(r: TaskSpecRaw) -> Result<Self, Self::Error> {
            let max_retries = match r.max_retries {
                None => None,
                Some(0) => {
                    return Err(ModelError::Invalid(
                        "maxRetries: 0 is not allowed; omit the field for an unlimited budget"
                            .into(),
                    ));
                }
                Some(n) => NonZeroU32::new(n),
            };

            let spec = Self {
                runner_selector: r.runner_selector,

                kind: r.kind,
                slot: r.slot,

                restart: r.restart,
                backoff: r.backoff,
                timeout: r.timeout,

                admission: r.admission,
                max_retries,
                labels: r.labels,
            };
            spec.validate_structural()?;
            Ok(spec)
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
    fn with_admission_overrides_policy() {
        let spec = valid_spec().with_admission(AdmissionPolicy::Replace);
        assert_eq!(spec.admission(), AdmissionPolicy::Replace);
    }

    #[test]
    fn serde_roundtrip() {
        let spec = valid_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: TaskSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn serde_rejects_empty_slot() {
        let spec = valid_spec();
        let mut json: serde_json::Value = serde_json::to_value(&spec).unwrap();
        json["slot"] = serde_json::Value::String(String::new());

        let err = serde_json::from_value::<TaskSpec>(json).unwrap_err();
        assert!(err.to_string().contains("slot"), "error: {err}");
    }

    #[test]
    fn serde_rejects_zero_timeout() {
        let spec = valid_spec();
        let mut json: serde_json::Value = serde_json::to_value(&spec).unwrap();
        json["timeout"] = serde_json::json!(0);

        let err = serde_json::from_value::<TaskSpec>(json).unwrap_err();
        assert!(err.to_string().contains("timeout"), "error: {err}");
    }

    #[test]
    fn serde_rejects_zero_max_retries() {
        let spec = valid_spec();
        let mut json: serde_json::Value = serde_json::to_value(&spec).unwrap();
        json["maxRetries"] = serde_json::json!(0);

        let err = serde_json::from_value::<TaskSpec>(json).unwrap_err();
        assert!(err.to_string().contains("maxRetries"), "error: {err}");
    }

    #[test]
    fn unlimited_max_retries_is_omitted_from_json() {
        let json = serde_json::to_value(valid_spec()).unwrap();
        assert!(
            json.get("maxRetries").is_none(),
            "unlimited budget must serialize as an absent field"
        );
    }

    #[test]
    fn max_retries_roundtrips_through_json() {
        let spec = valid_spec();
        let mut json: serde_json::Value = serde_json::to_value(&spec).unwrap();
        json["maxRetries"] = serde_json::json!(3);

        let back: TaskSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.max_retries().map(NonZeroU32::get), Some(3));
    }
}
