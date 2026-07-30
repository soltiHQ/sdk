//! # Task resource
//!
//! [`TaskManifest`] is caller-owned desired state.
//! [`Task`] is a stored resource with server metadata and status.
//!
//! ## Apply
//!
//! ```text
//! identical desired state ──▶ DesiredChange::None
//! labels or annotations   ──▶ DesiredChange::Metadata
//! spec changed            ──▶ DesiredChange::Spec
//!                              └─ generation increments
//! ```
//!
//! ## Status Flow
//!
//! ```text
//! Reconciled: Unknown        ── accepted     ────▶ True
//!             False          ── manual retry ────▶ Unknown
//!             Unknown | True ── failure      ────▶ False
//!
//! Pending ── attempt starts ──▶ Running ── attempt ends ──▶ terminal phase
//! ```
//!
//! Generation is checked before attempt transitions.
//! Stale generation updates are ignored.
//! Repeating an identical update is a no-op.

use serde::{Deserialize, Serialize};

use crate::{
    Annotations, ConditionStatus, Labels, ModelError, ModelResult, ObjectMeta, Slot, TaskCondition,
    TaskId, TaskPhase, TaskSpec, TaskStatus, Uid,
};

macro_rules! task_api_major {
    () => {
        1
    };
}

/// Major version of the built-in Task resource API.
pub const TASK_API_VERSION_MAJOR: u32 = task_api_major!();

/// API group and version of the built-in Task resource.
pub const TASK_API_VERSION: &str = concat!("solti.io/v", task_api_major!());

/// Kind of the built-in Task resource.
pub const TASK_KIND: &str = "Task";

/// Classification of an apply operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredChange {
    /// Desired state and user-owned metadata were already identical.
    None,
    /// Only labels and/or annotations changed.
    Metadata,
    /// Spec changed, optionally together with labels or annotations.
    Spec,
}

impl DesiredChange {
    /// Returns whether apply changed the resource.
    #[inline]
    pub fn is_changed(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Group/version and kind of resource schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TypeMeta {
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::task_api_version")
    )]
    api_version: String,
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::task_kind"))]
    kind: String,
}

/// User-owned metadata accepted in a [`TaskManifest`].
///
/// Runtime identity, resource version, generation, creation time and status are deliberately absent because the state store owns them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskManifestMeta {
    name: TaskId,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    labels: Labels,
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    annotations: Annotations,
}

impl TaskManifestMeta {
    /// Creates user-owned metadata.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `name` is invalid.
    pub fn new(name: impl AsRef<str>) -> ModelResult<Self> {
        let metadata = Self {
            name: TaskId::new(name)?,
            labels: Labels::new(),
            annotations: Annotations::new(),
        };
        metadata.name.validate_format()?;
        Ok(metadata)
    }

    /// Stable resource name.
    #[inline]
    pub fn name(&self) -> &TaskId {
        &self.name
    }

    /// Selector metadata.
    #[inline]
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Free-form metadata.
    #[inline]
    pub fn annotations(&self) -> &Annotations {
        &self.annotations
    }

    fn with_labels(mut self, labels: Labels) -> Self {
        self.labels = labels;
        self
    }

    fn with_annotations(mut self, annotations: Annotations) -> Self {
        self.annotations = annotations;
        self
    }

    fn validate(&self) -> ModelResult<()> {
        self.name.validate_format()?;
        self.labels.validate()?;
        self.annotations.validate()
    }
}

/// Caller-owned desired state for create and apply.
///
/// The serialized shape is `apiVersion`, `kind`, `metadata`, and `spec`.
/// A stored [`Task`] adds server metadata and `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "raw::TaskManifestRaw")]
pub struct TaskManifest {
    #[serde(flatten)]
    type_meta: TypeMeta,
    metadata: TaskManifestMeta,
    spec: TaskSpec,
}

impl TaskManifest {
    /// Creates a Task manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the name or spec is invalid.
    pub fn new(name: impl AsRef<str>, spec: TaskSpec) -> ModelResult<Self> {
        Self::from_parts(TypeMeta::task(), TaskManifestMeta::new(name)?, spec)
    }

    /// Reconstructs a manifest from serialized fields.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when GVK, metadata, or spec is invalid.
    pub fn from_parts(
        type_meta: TypeMeta,
        metadata: TaskManifestMeta,
        spec: TaskSpec,
    ) -> ModelResult<Self> {
        let manifest = Self {
            type_meta,
            metadata,
            spec,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Sets manifest labels.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when a label is invalid.
    pub fn with_labels(mut self, labels: Labels) -> ModelResult<Self> {
        labels.validate()?;
        self.metadata = self.metadata.with_labels(labels);
        Ok(self)
    }

    /// Sets manifest annotations.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when an annotation is invalid.
    pub fn with_annotations(mut self, annotations: Annotations) -> ModelResult<Self> {
        annotations.validate()?;
        self.metadata = self.metadata.with_annotations(annotations);
        Ok(self)
    }

    /// Validates the manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when GVK, metadata, or spec is invalid.
    pub fn validate(&self) -> ModelResult<()> {
        self.type_meta.validate_task()?;
        self.metadata.validate()?;
        self.spec.validate()
    }

    /// Resource type metadata.
    #[inline]
    pub fn type_meta(&self) -> &TypeMeta {
        &self.type_meta
    }

    /// User-owned resource metadata.
    #[inline]
    pub fn metadata(&self) -> &TaskManifestMeta {
        &self.metadata
    }

    /// Stable resource name.
    #[inline]
    pub fn name(&self) -> &TaskId {
        self.metadata.name()
    }

    /// Desired state.
    #[inline]
    pub fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    /// Logical concurrency slot.
    #[inline]
    pub fn slot(&self) -> &Slot {
        self.spec.slot()
    }

    /// Returns the serialized manifest fields.
    pub fn into_parts(self) -> (TypeMeta, TaskManifestMeta, TaskSpec) {
        (self.type_meta, self.metadata, self.spec)
    }
}

impl TypeMeta {
    /// Type metadata for the built-in Task resource.
    pub fn task() -> Self {
        Self {
            api_version: TASK_API_VERSION.to_owned(),
            kind: TASK_KIND.to_owned(),
        }
    }

    /// Resource API group and version.
    #[inline]
    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    /// Resource kind.
    #[inline]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    fn validate_task(&self) -> ModelResult<()> {
        if self.api_version != TASK_API_VERSION {
            return Err(ModelError::Invalid(
                format!(
                    "Task apiVersion must be `{TASK_API_VERSION}`, got `{}`",
                    self.api_version
                )
                .into(),
            ));
        }
        if self.kind != TASK_KIND {
            return Err(ModelError::Invalid(
                format!("Task kind must be `{TASK_KIND}`, got `{}`", self.kind).into(),
            ));
        }
        Ok(())
    }
}

/// Stored Task resource.
///
/// The serialized shape is `apiVersion`, `kind`, `metadata`, `spec`, `status`.
/// Name, UID, and creation time are preserved by [`Self::apply_desired`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, deny_unknown_fields))]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "raw::TaskRaw")]
pub struct Task {
    #[serde(flatten)]
    type_meta: TypeMeta,
    metadata: ObjectMeta,
    spec: TaskSpec,
    status: TaskStatus,
}

impl Task {
    /// Creates a stored Task with server-owned defaults.
    ///
    /// The UID and creation timestamp are generated, generation starts at `1`, and status starts as unobserved `Pending`.
    /// The state store assigns the initial resource version separately.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the name or spec is invalid, or the entropy source is unavailable.
    pub fn new(name: impl AsRef<str>, spec: TaskSpec) -> ModelResult<Self> {
        Self::from_manifest(TaskManifest::new(name, spec)?)
    }

    /// Creates a stored resource from a manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the manifest is invalid or the entropy source is unavailable.
    pub fn from_manifest(manifest: TaskManifest) -> ModelResult<Self> {
        manifest.validate()?;
        let (_, metadata, spec) = manifest.into_parts();
        let mut object_meta = ObjectMeta::new(metadata.name.clone())?;
        object_meta.apply_metadata(metadata.labels, metadata.annotations);
        let task = Self {
            type_meta: TypeMeta::task(),
            metadata: object_meta,
            spec,
            status: TaskStatus::pending(1)?,
        };
        task.validate()?;
        Ok(task)
    }

    /// Reconstructs a resource from persisted fields.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when a resource invariant is violated.
    pub fn from_parts(
        type_meta: TypeMeta,
        metadata: ObjectMeta,
        spec: TaskSpec,
        status: TaskStatus,
    ) -> ModelResult<Self> {
        let task = Self {
            type_meta,
            metadata,
            spec,
            status,
        };
        task.validate()?;
        Ok(task)
    }

    /// Validates the complete resource.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when GVK, metadata, spec, status, generation, or conditions are inconsistent.
    pub fn validate(&self) -> ModelResult<()> {
        self.type_meta.validate_task()?;
        self.metadata.name().validate_format()?;
        self.metadata.labels().validate()?;
        self.metadata.annotations().validate()?;
        if self.metadata.generation() == 0 {
            return Err(ModelError::Invalid(
                "metadata.generation must be greater than zero".into(),
            ));
        }
        if self.status.observed_generation > self.metadata.generation() {
            return Err(ModelError::Invalid(
                "status.observedGeneration cannot exceed metadata.generation".into(),
            ));
        }
        self.status.validate()?;
        if self
            .status
            .conditions()
            .iter()
            .any(|condition| condition.observed_generation() > self.metadata.generation())
        {
            return Err(ModelError::Invalid(
                "status.conditions[].observedGeneration cannot exceed metadata.generation".into(),
            ));
        }
        self.spec.validate()
    }

    /// Resource type metadata.
    #[inline]
    pub fn type_meta(&self) -> &TypeMeta {
        &self.type_meta
    }

    /// Resource metadata.
    #[inline]
    pub fn metadata(&self) -> &ObjectMeta {
        &self.metadata
    }

    /// Desired state.
    #[inline]
    pub fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    /// Observed state.
    #[inline]
    pub fn status(&self) -> &TaskStatus {
        &self.status
    }

    /// Returns the serialized resource fields.
    pub fn into_parts(self) -> (TypeMeta, ObjectMeta, TaskSpec, TaskStatus) {
        (self.type_meta, self.metadata, self.spec, self.status)
    }

    /// Stable resource address (`metadata.name`).
    #[inline]
    pub fn name(&self) -> &TaskId {
        self.metadata.name()
    }

    /// Identity of this resource incarnation.
    #[inline]
    pub fn uid(&self) -> &Uid {
        self.metadata.uid()
    }

    /// Logical concurrency slot.
    #[inline]
    pub fn slot(&self) -> &Slot {
        self.spec.slot()
    }

    /// Resource labels.
    #[inline]
    pub fn labels(&self) -> &Labels {
        self.metadata.labels()
    }

    /// Current lifecycle phase.
    #[inline]
    pub fn phase(&self) -> &TaskPhase {
        &self.status.phase
    }

    /// Assigns a state-store resource version.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is empty.
    pub fn set_resource_version(&mut self, resource_version: impl Into<String>) -> ModelResult<()> {
        self.metadata.set_resource_version(resource_version)
    }

    /// Applies caller-owned metadata and desired state.
    ///
    /// UID and creation time are preserved.
    /// Metadata-only changes preserve generation and status.
    /// Spec changes increment generation and reset phase and attempt.
    /// The previous `observedGeneration` is retained.
    ///
    /// Identical desired state returns [`DesiredChange::None`].
    /// In that case, `resource_version` is not assigned.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when metadata, spec, or a changed `resource_version` is invalid.
    pub fn apply_desired(
        &mut self,
        labels: Labels,
        annotations: Annotations,
        spec: TaskSpec,
        resource_version: impl Into<String>,
    ) -> ModelResult<DesiredChange> {
        spec.validate()?;
        labels.validate()?;
        annotations.validate()?;
        let metadata_changed =
            self.metadata.labels() != &labels || self.metadata.annotations() != &annotations;
        let spec_changed = self.spec != spec;
        if !metadata_changed && !spec_changed {
            return Ok(DesiredChange::None);
        }

        self.metadata.set_resource_version(resource_version)?;
        if metadata_changed {
            self.metadata.apply_metadata(labels, annotations);
        }
        if spec_changed {
            self.spec = spec;
            self.metadata.bump_generation();
            self.status = self.status.pending_after(self.metadata.generation());
        }
        Ok(if spec_changed {
            DesiredChange::Spec
        } else {
            DesiredChange::Metadata
        })
    }

    /// Marks the current generation as reconciled.
    ///
    /// Returns `true` when status changed.
    /// Returns `false` when the same generation was already reconciled.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when a changed `resource_version` is empty.
    pub fn mark_observed(&mut self, resource_version: impl Into<String>) -> ModelResult<bool> {
        let generation = self.metadata.generation();
        let condition = self.status.reconciled_required();
        let changed = self.status.observed_generation != generation
            || condition.status() != ConditionStatus::True
            || condition.observed_generation() != generation
            || condition.reason() != "RuntimeAccepted"
            || condition.message() != "runtime accepted the desired state";
        if !changed {
            return Ok(false);
        }
        self.metadata.set_resource_version(resource_version)?;
        self.status.mark_reconciled(generation);
        Ok(true)
    }

    /// Reschedules reconciliation after a recorded failure.
    ///
    /// Returns `false` when reconciliation is not failed.
    /// A change resets phase, attempt, exit code, and lifecycle error.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when a changed `resource_version` is empty.
    pub fn mark_reconciliation_pending(
        &mut self,
        resource_version: impl Into<String>,
    ) -> ModelResult<bool> {
        if !self.status.reconciliation_failed() {
            return Ok(false);
        }
        let generation = self.metadata.generation();
        self.metadata.set_resource_version(resource_version)?;
        self.status.mark_reconciliation_pending(generation);
        self.status.phase = TaskPhase::Pending;
        self.status.attempt = 0;
        self.status.exit_code = None;
        self.status.error = None;
        Ok(true)
    }

    /// Records a reconciliation failure.
    ///
    /// Desired state is retained.
    /// Execution phase and diagnostics are reset.
    ///
    /// Returns `true` when status changed.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when reason, message, or a changed `resource_version` is invalid.
    pub fn mark_reconciliation_failed(
        &mut self,
        reason: impl Into<String>,
        message: impl Into<String>,
        resource_version: impl Into<String>,
    ) -> ModelResult<bool> {
        let reason = reason.into();
        let message = message.into();
        TaskCondition::validate_reason_message(&reason, &message)?;
        let generation = self.metadata.generation();
        let condition = self.status.reconciled_required();
        let changed = condition.status() != ConditionStatus::False
            || condition.observed_generation() != generation
            || condition.reason() != reason
            || condition.message() != message
            || self.status.observed_generation != generation
            || self.status.phase != TaskPhase::Pending
            || self.status.attempt != 0
            || self.status.exit_code.is_some()
            || self.status.error.is_some();
        if !changed {
            return Ok(false);
        }
        self.metadata.set_resource_version(resource_version)?;
        self.status
            .mark_reconciliation_failed(generation, reason, message);
        self.status.phase = TaskPhase::Pending;
        self.status.attempt = 0;
        self.status.exit_code = None;
        self.status.error = None;
        Ok(true)
    }

    /// Records an authoritative attempt start.
    ///
    /// A stale generation returns `false`.
    /// An identical transition also returns `false`.
    /// Attempt numbers come from the execution source of truth.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the current generation has attempt zero or a changed `resource_version` is empty.
    pub fn transition_starting(
        &mut self,
        generation: u64,
        attempt: u32,
        resource_version: impl Into<String>,
    ) -> ModelResult<bool> {
        if generation != self.metadata.generation() {
            return Ok(false);
        }
        if attempt == 0 {
            return Err(ModelError::Invalid(
                "attempt must be greater than zero".into(),
            ));
        }
        let changed = self.status.observed_generation != generation
            || self.status.reconciled_required().status() != ConditionStatus::True
            || self.status.reconciled_required().observed_generation() != generation
            || self.status.phase != TaskPhase::Running
            || self.status.attempt != attempt
            || self.status.exit_code.is_some()
            || self.status.error.is_some();
        if !changed {
            return Ok(false);
        }
        self.metadata.set_resource_version(resource_version)?;
        self.status.mark_reconciled(generation);
        self.status.phase = TaskPhase::Running;
        self.status.attempt = attempt;
        self.status.exit_code = None;
        self.status.error = None;
        Ok(true)
    }

    /// Records a terminal attempt phase.
    ///
    /// A stale generation or older attempt returns `false`.
    /// Terminal phases are sticky.
    /// `Failed` may be refined to `Exhausted` or `Timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the current generation has attempt zero, `phase` is not terminal, or a changed `resource_version` is empty.
    pub fn transition_finished(
        &mut self,
        generation: u64,
        attempt: u32,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        resource_version: impl Into<String>,
    ) -> ModelResult<bool> {
        if generation != self.metadata.generation() {
            return Ok(false);
        }
        if attempt == 0 {
            return Err(ModelError::Invalid(
                "attempt must be greater than zero".into(),
            ));
        }
        if !phase.is_terminal() {
            return Err(ModelError::Invalid(
                format!("transition_finished requires a terminal phase, got {phase}").into(),
            ));
        }
        if attempt < self.status.attempt {
            return Ok(false);
        }
        let same_attempt = attempt == self.status.attempt;
        let reconciled = self.status.reconciled_required().status() == ConditionStatus::True
            && self.status.reconciled_required().observed_generation() == generation;
        if same_attempt && self.status.phase == phase && reconciled {
            return Ok(false);
        }
        if same_attempt && self.status.phase.is_terminal() && reconciled {
            let refines_failed = self.status.phase == TaskPhase::Failed
                && matches!(phase, TaskPhase::Exhausted | TaskPhase::Timeout);
            if !refines_failed {
                return Ok(false);
            }
        }
        self.metadata.set_resource_version(resource_version)?;
        self.status.attempt = attempt;
        self.set_terminal(generation, phase, error, exit_code);
        Ok(true)
    }

    /// Records an authoritative final lifecycle outcome.
    ///
    /// Unlike [`Self::transition_finished`], this may replace a conflicting terminal attempt phase.
    /// A stale generation or identical outcome returns `false`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when `phase` is not terminal for the current generation or a changed `resource_version` is empty.
    pub fn reconcile_finished(
        &mut self,
        generation: u64,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        resource_version: impl Into<String>,
    ) -> ModelResult<bool> {
        if generation != self.metadata.generation() {
            return Ok(false);
        }
        if !phase.is_terminal() {
            return Err(ModelError::Invalid(
                format!("reconcile_finished requires a terminal phase, got {phase}").into(),
            ));
        }
        let changed = self.status.observed_generation != generation
            || self.status.reconciled_required().status() != ConditionStatus::True
            || self.status.reconciled_required().observed_generation() != generation
            || self.status.phase != phase
            || self.status.error != error
            || self.status.exit_code != exit_code;
        if !changed {
            return Ok(false);
        }
        self.metadata.set_resource_version(resource_version)?;
        self.set_terminal(generation, phase, error, exit_code);
        Ok(true)
    }

    fn set_terminal(
        &mut self,
        generation: u64,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) {
        self.status.mark_reconciled(generation);
        self.status.phase = phase;
        self.status.error = error;
        self.status.exit_code = exit_code;
    }
}

impl From<&Task> for TaskManifest {
    fn from(task: &Task) -> Self {
        Self {
            type_meta: task.type_meta.clone(),
            metadata: TaskManifestMeta {
                name: task.name().clone(),
                labels: task.metadata.labels().clone(),
                annotations: task.metadata.annotations().clone(),
            },
            spec: task.spec.clone(),
        }
    }
}

impl From<Task> for TaskManifest {
    fn from(task: Task) -> Self {
        Self::from(&task)
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskRaw {
        api_version: String,
        kind: String,
        metadata: ObjectMeta,
        spec: TaskSpec,
        status: TaskStatus,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct TaskManifestRaw {
        api_version: String,
        kind: String,
        metadata: TaskManifestMeta,
        spec: TaskSpec,
    }

    impl TryFrom<TaskRaw> for Task {
        type Error = ModelError;

        fn try_from(raw: TaskRaw) -> Result<Self, Self::Error> {
            Task::from_parts(
                TypeMeta {
                    api_version: raw.api_version,
                    kind: raw.kind,
                },
                raw.metadata,
                raw.spec,
                raw.status,
            )
        }
    }

    impl TryFrom<TaskManifestRaw> for TaskManifest {
        type Error = ModelError;

        fn try_from(raw: TaskManifestRaw) -> Result<Self, Self::Error> {
            TaskManifest::from_parts(
                TypeMeta {
                    api_version: raw.api_version,
                    kind: raw.kind,
                },
                raw.metadata,
                raw.spec,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedSpec, TaskWorkload};

    fn spec(slot: &str) -> TaskSpec {
        TaskSpec::builder(
            slot,
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
            5_000_u64,
        )
        .build()
        .unwrap()
    }

    fn task() -> Task {
        let mut task = Task::new("task-a", spec("slot-a")).unwrap();
        task.set_resource_version("1").unwrap();
        task
    }

    #[test]
    fn new_creates_valid_unobserved_resource() {
        let task = Task::new("task-a", spec("slot-a")).unwrap();

        assert_eq!(task.type_meta().api_version(), TASK_API_VERSION);
        assert_eq!(task.type_meta().kind(), TASK_KIND);
        assert_eq!(task.name(), "task-a");
        assert!(!task.uid().as_str().is_empty());
        assert_eq!(task.metadata().generation(), 1);
        assert_eq!(task.status().observed_generation(), 0);
        assert_eq!(*task.phase(), TaskPhase::Pending);
        assert_eq!(
            task.status().reconciled().status(),
            ConditionStatus::Unknown
        );
        assert_eq!(task.status().reconciled().observed_generation(), 1);
    }

    #[test]
    fn serde_shape_and_roundtrip_are_crd_shaped() {
        let task = task();
        let json = serde_json::to_value(&task).unwrap();

        assert_eq!(json["apiVersion"], TASK_API_VERSION);
        assert_eq!(json["kind"], TASK_KIND);
        assert!(json.get("metadata").is_some());
        assert!(json.get("spec").is_some());
        assert!(json.get("status").is_some());

        let back: Task = serde_json::from_value(json).unwrap();
        assert_eq!(back, task);
    }

    #[test]
    fn manifest_serde_contains_only_user_owned_resource_fields() {
        let mut labels = Labels::new();
        labels.insert("tier", "worker");
        let manifest = TaskManifest::new("task-a", spec("slot-a"))
            .unwrap()
            .with_labels(labels)
            .unwrap();

        let json = serde_json::to_value(&manifest).unwrap();
        assert_eq!(json["apiVersion"], TASK_API_VERSION);
        assert_eq!(json["kind"], TASK_KIND);
        assert_eq!(json["metadata"]["name"], "task-a");
        assert!(json.get("spec").is_some());
        assert!(json.get("status").is_none());
        for server_owned in ["uid", "resourceVersion", "generation", "creationTimestamp"] {
            assert!(json["metadata"].get(server_owned).is_none());
        }

        let back: TaskManifest = serde_json::from_value(json).unwrap();
        assert_eq!(back, manifest);
    }

    #[test]
    fn stored_task_roundtrips_through_its_desired_manifest() {
        let stored = task();
        let manifest = TaskManifest::from(&stored);
        let rematerialized = Task::from_manifest(manifest).unwrap();

        assert_eq!(rematerialized.name(), stored.name());
        assert_eq!(rematerialized.spec(), stored.spec());
        assert_eq!(
            rematerialized.metadata().labels(),
            stored.metadata().labels()
        );
        assert_ne!(rematerialized.uid(), stored.uid());
        assert_eq!(rematerialized.status().phase(), TaskPhase::Pending);
    }

    #[test]
    fn manifest_deserialization_rejects_wrong_resource_gvk() {
        let manifest = TaskManifest::new("task-a", spec("slot-a")).unwrap();
        let mut json = serde_json::to_value(manifest).unwrap();
        json["kind"] = serde_json::json!("Other");

        let error = serde_json::from_value::<TaskManifest>(json).unwrap_err();
        assert!(error.to_string().contains("Task kind"));
    }

    #[test]
    fn manifest_deserialization_rejects_invalid_metadata() {
        let manifest = TaskManifest::new("task-a", spec("slot-a")).unwrap();
        let mut json = serde_json::to_value(manifest).unwrap();
        json["metadata"]["labels"] = serde_json::json!({ "example.io/bad key": "value" });

        let error = serde_json::from_value::<TaskManifest>(json).unwrap_err();
        assert!(error.to_string().contains("label key"));
    }

    #[test]
    fn manifest_rejects_unknown_fields_at_resource_and_metadata_levels() {
        let manifest = TaskManifest::new("task-a", spec("slot-a")).unwrap();
        let mut resource = serde_json::to_value(&manifest).unwrap();
        resource["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskManifest>(resource).is_err());

        let mut metadata = serde_json::to_value(manifest).unwrap();
        metadata["metadata"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TaskManifest>(metadata).is_err());
    }

    #[test]
    fn stored_task_rejects_unknown_status_and_server_metadata_fields() {
        let stored = task();
        let mut metadata = serde_json::to_value(&stored).unwrap();
        metadata["metadata"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Task>(metadata).is_err());

        let mut status = serde_json::to_value(stored).unwrap();
        status["status"]["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Task>(status).is_err());
    }

    #[test]
    fn serde_rejects_wrong_resource_gvk() {
        let mut json = serde_json::to_value(task()).unwrap();
        json["kind"] = serde_json::json!("Other");

        let error = serde_json::from_value::<Task>(json).unwrap_err();
        assert!(error.to_string().contains("Task kind"));
    }

    #[test]
    fn metadata_only_apply_preserves_generation_and_status() {
        let mut task = task();
        let uid = task.uid().clone();
        let creation = task.metadata().creation_timestamp();
        let mut labels = Labels::new();
        labels.insert("tier", "prod");

        let changed = task
            .apply_desired(labels, Annotations::new(), spec("slot-a"), "2")
            .unwrap();

        assert_eq!(changed, DesiredChange::Metadata);
        assert_eq!(task.metadata().generation(), 1);
        assert_eq!(task.uid(), &uid);
        assert_eq!(task.metadata().creation_timestamp(), creation);
        assert_eq!(task.metadata().resource_version(), "2");
        assert_eq!(*task.phase(), TaskPhase::Pending);
    }

    #[test]
    fn spec_apply_increments_generation_and_resets_execution_state() {
        let mut task = task();
        task.transition_starting(1, 4, "2").unwrap();

        let change = task
            .apply_desired(Labels::new(), Annotations::new(), spec("slot-b"), "3")
            .unwrap();

        assert_eq!(change, DesiredChange::Spec);
        assert_eq!(task.metadata().generation(), 2);
        assert_eq!(task.status().observed_generation(), 1);
        assert_eq!(task.status().attempt(), 0);
        assert_eq!(*task.phase(), TaskPhase::Pending);
        assert_eq!(
            task.status().reconciled().status(),
            ConditionStatus::Unknown
        );
        assert_eq!(task.status().reconciled().observed_generation(), 2);
        assert_eq!(task.slot(), "slot-b");
    }

    #[test]
    fn embedded_revision_change_is_a_spec_change() {
        let mut task = task();
        let transition_time = task.status().reconciled().last_transition_time();
        let changed_spec = TaskSpec::builder(
            "slot-a",
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v2").unwrap()),
            5_000_u64,
        )
        .build()
        .unwrap();

        let change = task
            .apply_desired(Labels::new(), Annotations::new(), changed_spec, "2")
            .unwrap();

        assert_eq!(change, DesiredChange::Spec);
        assert_eq!(task.metadata().generation(), 2);
        let TaskWorkload::Embedded(embedded) = task.spec().workload() else {
            panic!("workload must remain Embedded");
        };
        assert_eq!(embedded.revision(), "test-v2");
        assert_eq!(
            task.status().reconciled().last_transition_time(),
            transition_time,
            "lastTransitionTime changes only when condition status changes"
        );
    }

    #[test]
    fn starting_uses_authoritative_attempt_and_observes_generation() {
        let mut task = task();

        assert!(task.transition_starting(1, 7, "2").unwrap());
        assert_eq!(task.status().attempt(), 7);
        assert_eq!(task.status().observed_generation(), 1);
        assert_eq!(*task.phase(), TaskPhase::Running);
    }

    #[test]
    fn terminal_event_sets_attempt_when_start_event_was_not_observed() {
        let mut task = task();

        task.transition_finished(1, 3, TaskPhase::Succeeded, None, Some(0), "2")
            .unwrap();

        assert_eq!(task.status().attempt(), 3);
        assert_eq!(task.status().observed_generation(), 1);
        assert_eq!(*task.phase(), TaskPhase::Succeeded);
    }

    #[test]
    fn no_op_apply_does_not_consume_resource_version() {
        let mut task = task();

        let change = task
            .apply_desired(Labels::new(), Annotations::new(), spec("slot-a"), "2")
            .unwrap();

        assert_eq!(change, DesiredChange::None);
        assert_eq!(task.metadata().resource_version(), "1");
    }

    #[test]
    fn invalid_metadata_apply_is_rejected_without_mutation() {
        let mut task = task();
        let before = task.clone();
        let mut labels = Labels::new();
        labels.insert("bad key", "value");

        assert!(
            task.apply_desired(labels, Annotations::new(), spec("slot-b"), "2")
                .is_err()
        );
        assert_eq!(task, before);
    }

    #[test]
    fn stale_generation_status_is_ignored_without_bumping_version() {
        let mut task = task();

        assert!(!task.transition_starting(2, 1, "2").unwrap());
        assert_eq!(task.metadata().resource_version(), "1");
        assert_eq!(*task.phase(), TaskPhase::Pending);
    }

    #[test]
    fn reconciliation_failure_observes_generation_and_retains_spec() {
        let mut task = task();

        assert!(
            task.mark_reconciliation_failed("RunnerBuildFailed", "no runner", "2")
                .unwrap()
        );
        assert_eq!(task.status().observed_generation(), 1);
        assert_eq!(*task.phase(), TaskPhase::Pending);
        assert_eq!(task.status().attempt(), 0);
        assert!(task.status().error().is_none());
        assert_eq!(task.status().reconciled().status(), ConditionStatus::False);
        assert_eq!(task.status().reconciled().reason(), "RunnerBuildFailed");
        assert_eq!(task.status().reconciled().message(), "no runner");
        assert_eq!(task.slot(), "slot-a");
    }

    #[test]
    fn failed_reconciliation_can_be_rescheduled_without_changing_generation() {
        let mut task = task();
        task.mark_reconciliation_failed("RunnerBuildFailed", "no runner", "2")
            .unwrap();

        assert!(task.mark_reconciliation_pending("3").unwrap());
        assert_eq!(task.metadata().generation(), 1);
        assert_eq!(task.metadata().resource_version(), "3");
        assert_eq!(
            task.status().reconciled().status(),
            ConditionStatus::Unknown
        );
        assert_eq!(task.status().reconciled().observed_generation(), 1);
    }

    #[test]
    fn sticky_terminal_can_only_refine_failed() {
        let mut task = task();
        task.transition_starting(1, 1, "2").unwrap();
        task.transition_finished(1, 1, TaskPhase::Failed, Some("attempt".into()), None, "3")
            .unwrap();

        assert!(
            !task
                .transition_finished(1, 1, TaskPhase::Succeeded, None, Some(0), "4")
                .unwrap()
        );
        assert!(
            task.transition_finished(1, 1, TaskPhase::Exhausted, Some("budget".into()), None, "4",)
                .unwrap()
        );
        assert_eq!(*task.phase(), TaskPhase::Exhausted);
    }
}
