//! Task specification.
//!
//! [`TaskSpec`] defines desired state: what to run and how the supervisor should manage it.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{
    AdmissionPolicy, BackoffPolicy, LabelSelector, RestartPolicy, Slot, TaskWorkload, Timeout,
    error::{ModelError, ModelResult},
};

/// Desired state for a task.
///
/// Build it with [`TaskSpec::builder`]. Fields are private so every spec goes through validation.
///
/// ## Example
///
/// ```
/// use solti_model::{EmbeddedSpec, RestartPolicy, TaskSpec, TaskWorkload};
///
/// let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
/// let spec = TaskSpec::builder("daily-cleanup", workload, 5_000u64)
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
    workload: TaskWorkload,

    timeout: Timeout,
    restart: RestartPolicy,
    backoff: BackoffPolicy,
    admission: AdmissionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_retries: Option<NonZeroU32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    runner_selector: Option<LabelSelector>,
}

impl TaskSpec {
    /// Logical slot name for concurrency control.
    #[inline]
    pub fn slot(&self) -> &Slot {
        &self.slot
    }

    /// Execution backend used to run the task.
    #[inline]
    pub fn workload(&self) -> &TaskWorkload {
        &self.workload
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
    pub fn runner_selector(&self) -> Option<&LabelSelector> {
        self.runner_selector.as_ref()
    }
}

impl TaskSpec {
    /// Create a [`TaskSpecBuilder`] with the three required fields.
    ///
    /// ```rust
    /// use solti_model::{TaskSpec, TaskWorkload, SubprocessSpec, SubprocessMode, RestartPolicy};
    ///
    /// let spec = TaskSpec::builder(
    ///     "my-slot",
    ///     TaskWorkload::Subprocess(SubprocessSpec::new(
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
        slot: impl AsRef<str>,
        workload: TaskWorkload,
        timeout: impl Into<u64>,
    ) -> TaskSpecBuilder {
        TaskSpecBuilder::new(slot, workload, timeout)
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
    /// use solti_model::{EmbeddedSpec, Labels, LabelSelector, TaskSpec, TaskWorkload};
    ///
    /// let mut labels = Labels::new();
    /// labels.insert("zone", "eu");
    ///
    /// let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
    /// let spec = TaskSpec::builder("build", workload, 1_000u64)
    ///     .build()
    ///     .unwrap()
    ///     .with_runner_selector(LabelSelector::from_labels(labels));
    ///
    /// assert!(spec.runner_selector().is_some());
    /// ```
    #[inline]
    pub fn with_runner_selector(mut self, sel: LabelSelector) -> Self {
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
    /// use solti_model::{AdmissionPolicy, EmbeddedSpec, TaskSpec, TaskWorkload};
    ///
    /// let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
    /// let spec = TaskSpec::builder("agent", workload, 1_000u64)
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
    /// Runs structural validation of shared and workload-specific fields.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{
    ///     Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskSpec, TaskWorkload,
    /// };
    ///
    /// let spec = TaskSpec::builder(
    ///     "hello",
    ///     TaskWorkload::Subprocess(SubprocessSpec::new(
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
        self.validate_structural()
    }

    /// Structural validation of all fields.
    ///
    /// Checks:
    /// - `slot` is not empty
    /// - `backoff` parameters are sane
    /// - `timeout` is greater than zero
    /// - workload-specific constraints (e.g. non-empty command)
    /// - `runner_selector` requirements are structurally valid
    fn validate_structural(&self) -> ModelResult<()> {
        self.slot.validate_format()?;
        self.workload.validate()?;
        self.backoff.validate()?;
        if let Some(ref sel) = self.runner_selector {
            sel.validate()?;
        }
        Ok(())
    }
}

/// Builder for [`TaskSpec`].
///
/// Required fields (`slot`, `workload`, `timeout`) are set in the constructor.
/// Optional fields have sensible defaults:
/// - `backoff`: [`BackoffPolicy::default`] (full jitter, 1 second to 30 seconds, factor 2)
/// - `admission`: [`AdmissionPolicy::DropIfRunning`]
/// - `restart`: [`RestartPolicy::Never`]
/// - `runner_selector`: `None`
///
/// ## Example
///
/// ```
/// use solti_model::{AdmissionPolicy, EmbeddedSpec, RestartPolicy, TaskSpec, TaskWorkload};
///
/// let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
/// let spec = TaskSpec::builder("service", workload, 5_000u64)
///     .restart(RestartPolicy::always())
///     .admission(AdmissionPolicy::Replace)
///     .build()
///     .unwrap();
///
/// assert_eq!(spec.restart(), RestartPolicy::always());
/// assert_eq!(spec.admission(), AdmissionPolicy::Replace);
/// ```
pub struct TaskSpecBuilder {
    runner_selector: Option<LabelSelector>,

    workload: TaskWorkload,
    slot: String,

    backoff: BackoffPolicy,
    restart: RestartPolicy,
    timeout_ms: u64,
    max_retries: Option<NonZeroU32>,

    admission: AdmissionPolicy,
}

impl TaskSpecBuilder {
    fn new(slot: impl AsRef<str>, workload: TaskWorkload, timeout: impl Into<u64>) -> Self {
        Self {
            runner_selector: None,

            workload,
            slot: slot.as_ref().to_owned(),

            restart: RestartPolicy::default(),
            backoff: BackoffPolicy::default(),
            timeout_ms: timeout.into(),

            admission: AdmissionPolicy::default(),
            max_retries: None,
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
    /// # use solti_model::{EmbeddedSpec, TaskSpec, TaskWorkload};
    /// # use std::num::NonZeroU32;
    /// let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
    /// let spec = TaskSpec::builder("s", workload, 1_000u64)
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
    pub fn runner_selector(mut self, sel: LabelSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }

    /// Build the [`TaskSpec`], validating structural invariants.
    ///
    /// Runner-specific routability is intentionally enforced by `solti-runner`,
    /// not by the shared resource model.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{EmbeddedSpec, TaskSpec, TaskWorkload};
    ///
    /// let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
    /// let err = TaskSpec::builder("", workload, 1_000u64)
    ///     .build()
    ///     .unwrap_err();
    ///
    /// assert!(err.to_string().contains("slot"));
    /// ```
    pub fn build(self) -> ModelResult<TaskSpec> {
        let spec = TaskSpec {
            runner_selector: self.runner_selector,

            workload: self.workload,
            slot: Slot::new(self.slot)?,

            restart: self.restart,
            backoff: self.backoff,
            timeout: Timeout::new(self.timeout_ms)?,

            admission: self.admission,
            max_retries: self.max_retries,
        };
        spec.validate_structural()?;
        Ok(spec)
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskSpecRaw {
        slot: Slot,
        workload: TaskWorkload,
        timeout: Timeout,
        restart: RestartPolicy,
        backoff: BackoffPolicy,
        admission: AdmissionPolicy,
        #[serde(default)]
        max_retries: Option<u32>,

        #[serde(default)]
        runner_selector: Option<LabelSelector>,
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

                workload: r.workload,
                slot: r.slot,

                restart: r.restart,
                backoff: r.backoff,
                timeout: r.timeout,

                admission: r.admission,
                max_retries,
            };
            spec.validate_structural()?;
            Ok(spec)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedSpec, Flag, SubprocessMode, SubprocessSpec, TaskEnv};

    fn embedded() -> TaskWorkload {
        TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap())
    }

    fn valid_spec() -> TaskSpec {
        TaskSpec::builder(
            "test",
            TaskWorkload::Subprocess(SubprocessSpec {
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
        let err = TaskSpec::builder("", embedded(), 5_000u64)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("slot"));
    }

    #[test]
    fn builder_rejects_zero_timeout() {
        let err = TaskSpec::builder("test", embedded(), 0u64)
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("timeout"));
    }

    #[test]
    fn builder_allows_embedded_kind() {
        let spec = TaskSpec::builder("test", embedded(), 5_000u64)
            .build()
            .expect("Embedded is structurally valid");
        assert!(matches!(spec.workload(), TaskWorkload::Embedded(_)));
    }

    #[test]
    fn validate_accepts_embedded_workload_as_structural_data() {
        let spec = TaskSpec::builder("test", embedded(), 5_000u64)
            .build()
            .unwrap();
        spec.validate().unwrap();
    }

    #[test]
    fn getters_return_expected_values() {
        let spec = TaskSpec::builder("my-slot", embedded(), 10_000u64)
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
    fn serde_rejects_unknown_fields() {
        let mut json = serde_json::to_value(valid_spec()).unwrap();
        json["unexpected"] = serde_json::json!(true);

        assert!(serde_json::from_value::<TaskSpec>(json).is_err());
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
