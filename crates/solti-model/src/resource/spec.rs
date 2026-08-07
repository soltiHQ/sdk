//! # Task spec
//!
//! [`TaskSpec`] defines workload, slot, timeout, policies, and runner selection.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::{
    AdmissionPolicy, BackoffPolicy, LabelSelector, RestartPolicy, Slot, TaskWorkload, Timeout,
    error::{ModelError, ModelResult},
};

/// Desired state for a task.
///
/// Use [`TaskSpec::builder`] to construct it.
/// Resource constructors validate the completed spec.
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, deny_unknown_fields))]
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

    /// Workload desired state.
    #[inline]
    pub fn workload(&self) -> &TaskWorkload {
        &self.workload
    }

    /// Per-attempt timeout.
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

    /// Maximum consecutive failure retries.
    ///
    /// `None` means unlimited.
    /// Zero is not representable.
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
    /// Creates a builder with the required fields.
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
    /// Replaces the workload on an existing spec.
    ///
    /// This method does not validate `workload`.
    /// Call [`Self::validate`] before using the result outside a validated resource.
    ///
    /// This is useful for composite runners that derive an execution view from an existing task while preserving its lifecycle policies.
    #[inline]
    pub fn with_workload(mut self, workload: TaskWorkload) -> Self {
        self.workload = workload;
        self
    }

    /// Sets a runner selector on an existing spec.
    ///
    /// This method does not validate `sel`.
    /// Call [`Self::validate`] before using the result outside a validated resource.
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

    /// Removes the runner selector from an existing spec.
    ///
    /// Composite runners can use this before applying a step-local selector.
    /// The selector that chose the composite runner is not inherited by a nested workload.
    #[inline]
    pub fn without_runner_selector(mut self) -> Self {
        self.runner_selector = None;
        self
    }

    /// Sets the admission policy on an existing spec.
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
    /// Validates the complete spec.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the slot, workload, backoff, or runner selector is invalid.
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

    /// Validates all structural fields.
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
/// Required fields are set by [`TaskSpec::builder`].
///
/// Optional fields use these defaults:
///
/// - `backoff`: [`BackoffPolicy::default`] (full jitter, 1 second to 30 seconds, factor 2)
/// - `admission`: [`AdmissionPolicy::DropIfRunning`]
/// - `restart`: [`RestartPolicy::Never`]
/// - `max_retries`: `None`
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

    /// Sets the restart policy.
    #[must_use]
    pub fn restart(mut self, restart: RestartPolicy) -> Self {
        self.restart = restart;
        self
    }

    /// Sets the failure-retry budget.
    ///
    /// `None` means unlimited.
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

    /// Sets the backoff policy.
    #[must_use]
    pub fn backoff(mut self, backoff: BackoffPolicy) -> Self {
        self.backoff = backoff;
        self
    }

    /// Sets the admission policy.
    #[must_use]
    pub fn admission(mut self, admission: AdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Sets the runner selector.
    #[must_use]
    pub fn runner_selector(mut self, sel: LabelSelector) -> Self {
        self.runner_selector = Some(sel);
        self
    }

    /// Builds and validates the spec.
    ///
    /// Runner availability is not checked by this crate.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when any structural field is invalid.
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
    fn builder_accepts_valid_specs_and_rejects_required_field_errors() {
        valid_spec().validate().unwrap();

        for (slot, timeout, field) in [("", 5_000_u64, "slot"), ("test", 0_u64, "timeout")] {
            let error = TaskSpec::builder(slot, embedded(), timeout)
                .build()
                .unwrap_err();
            assert!(error.to_string().contains(field), "got: {error}");
        }
    }

    #[test]
    fn embedded_workload_is_structurally_valid() {
        let spec = TaskSpec::builder("test", embedded(), 5_000u64)
            .build()
            .expect("Embedded is structurally valid");
        assert!(matches!(spec.workload(), TaskWorkload::Embedded(_)));
        spec.validate().unwrap();
    }

    #[test]
    fn builder_and_override_methods_expose_expected_values() {
        let spec = TaskSpec::builder("my-slot", embedded(), 10_000u64)
            .restart(RestartPolicy::OnFailure)
            .admission(AdmissionPolicy::Replace)
            .build()
            .unwrap();

        assert_eq!(spec.slot(), "my-slot");
        assert_eq!(spec.timeout().as_millis(), 10_000);
        assert_eq!(spec.restart(), RestartPolicy::OnFailure);
        assert_eq!(spec.admission(), AdmissionPolicy::Replace);
        assert_eq!(
            valid_spec()
                .with_admission(AdmissionPolicy::Replace)
                .admission(),
            AdmissionPolicy::Replace
        );

        let mut labels = crate::Labels::new();
        labels.insert("runtime", "nested");
        let selector = LabelSelector::from_labels(labels);
        let replaced = valid_spec()
            .with_runner_selector(selector)
            .with_workload(embedded())
            .without_runner_selector();
        assert!(matches!(replaced.workload(), TaskWorkload::Embedded(_)));
        assert!(replaced.runner_selector().is_none());
    }

    #[test]
    fn serde_roundtrip_and_unlimited_retry_shape_are_stable() {
        let spec = valid_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: TaskSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
        let json = serde_json::to_value(valid_spec()).unwrap();
        assert!(
            json.get("maxRetries").is_none(),
            "unlimited budget must serialize as an absent field"
        );
    }

    #[test]
    fn serde_validates_fields_and_rejects_unknown_fields() {
        for (field, value, expected) in [
            ("slot", serde_json::json!(""), "slot"),
            ("timeout", serde_json::json!(0), "timeout"),
            ("maxRetries", serde_json::json!(0), "maxRetries"),
        ] {
            let mut json = serde_json::to_value(valid_spec()).unwrap();
            json[field] = value;
            let error = serde_json::from_value::<TaskSpec>(json).unwrap_err();
            assert!(error.to_string().contains(expected), "got: {error}");
        }

        let mut json = serde_json::to_value(valid_spec()).unwrap();
        json["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskSpec>(json).is_err());
    }

    #[test]
    fn finite_retry_budget_roundtrips_through_json() {
        let spec = valid_spec();
        let mut json: serde_json::Value = serde_json::to_value(&spec).unwrap();
        json["maxRetries"] = serde_json::json!(3);

        let back: TaskSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.max_retries().map(NonZeroU32::get), Some(3));
    }
}
