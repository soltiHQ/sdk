//! Chain workload desired-state types and graph validation.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use solti_model::{ExtensionWorkload, LabelSelector, TaskId, TaskWorkload};

use crate::{ChainError, ChainResult};

/// API group and version of the chain extension workload.
pub const CHAIN_API_VERSION: &str = "chain.solti.io/v1alpha1";

/// Kind of the chain extension workload.
pub const CHAIN_KIND: &str = "Chain";

/// Failure-state behavior after following an `onFailure` transition.
///
/// `Preserve` is the wire default, so omitting `mode` has the same meaning as writing `mode: preserve`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum FailureMode {
    /// Preserve the failure that selected the transition.
    #[default]
    Preserve,

    /// Allow a successful failure-handler path to recover the chain.
    Recover,
}

/// Transition selected when a step fails.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(deny_unknown_fields))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailureTransition {
    next: TaskId,

    #[serde(default)]
    mode: FailureMode,
}

impl FailureTransition {
    /// Creates a transition to `next` with the selected failure mode.
    ///
    /// Step names use the existing [`TaskId`] validator.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`] when `next` is not a valid [`TaskId`].
    pub fn new(next: impl AsRef<str>, mode: FailureMode) -> ChainResult<Self> {
        Ok(Self {
            next: step_name("onFailure.next", next)?,
            mode,
        })
    }

    /// Name of the next step.
    #[inline]
    pub fn next(&self) -> &TaskId {
        &self.next
    }

    /// Failure-state behavior for the transition.
    #[inline]
    pub fn mode(&self) -> FailureMode {
        self.mode
    }
}

/// One executable step in a [`ChainSpec`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, deny_unknown_fields))]
#[serde(rename_all = "camelCase", try_from = "raw::ChainStepRaw")]
pub struct ChainStep {
    name: TaskId,

    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::chain_step_workload")
    )]
    workload: TaskWorkload,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    runner_selector: Option<LabelSelector>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_success: Option<TaskId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_failure: Option<FailureTransition>,
}

impl ChainStep {
    /// Creates a step without outgoing transitions or a runner selector.
    ///
    /// Step names use the existing [`TaskId`] validator.
    /// Built-in `Embedded` workloads and a nested workload with this crate's chain GVK are rejected.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`] when the name or workload is invalid or the workload is forbidden inside a v1alpha1 chain.
    pub fn new(name: impl AsRef<str>, workload: TaskWorkload) -> ChainResult<Self> {
        let step = Self {
            name: step_name("step.name", name)?,
            workload,
            runner_selector: None,
            on_success: None,
            on_failure: None,
        };
        step.validate()?;
        Ok(step)
    }

    /// Sets the selector used to route this step's workload.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`] when the selector is invalid.
    pub fn with_runner_selector(mut self, selector: LabelSelector) -> ChainResult<Self> {
        selector.validate().map_err(|error| {
            ChainError::Invalid(format!(
                "step '{}'.runnerSelector is invalid: {error}",
                self.name
            ))
        })?;
        self.runner_selector = Some(selector);
        Ok(self)
    }

    /// Sets the step selected after successful completion.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`] when `next` is not a valid step name.
    pub fn with_on_success(mut self, next: impl AsRef<str>) -> ChainResult<Self> {
        self.on_success = Some(step_name("onSuccess", next)?);
        Ok(self)
    }

    /// Sets the step selected after failure.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`] when `next` is not a valid step name.
    pub fn with_on_failure(
        mut self,
        next: impl AsRef<str>,
        mode: FailureMode,
    ) -> ChainResult<Self> {
        self.on_failure = Some(FailureTransition::new(next, mode)?);
        Ok(self)
    }

    /// Sets a pre-built failure transition.
    #[must_use]
    pub fn with_failure_transition(mut self, transition: FailureTransition) -> Self {
        self.on_failure = Some(transition);
        self
    }

    /// Stable step name.
    #[inline]
    pub fn name(&self) -> &TaskId {
        &self.name
    }

    /// Workload executed by the step.
    #[inline]
    pub fn workload(&self) -> &TaskWorkload {
        &self.workload
    }

    /// Optional runner selector applied only to this step.
    #[inline]
    pub fn runner_selector(&self) -> Option<&LabelSelector> {
        self.runner_selector.as_ref()
    }

    /// Name of the step selected after success, if configured.
    #[inline]
    pub fn on_success(&self) -> Option<&TaskId> {
        self.on_success.as_ref()
    }

    /// Transition selected after failure, if configured.
    #[inline]
    pub fn on_failure(&self) -> Option<&FailureTransition> {
        self.on_failure.as_ref()
    }

    fn validate(&self) -> ChainResult<()> {
        self.name.validate_format().map_err(|error| {
            ChainError::Invalid(format!("step name '{}' is invalid: {error}", self.name))
        })?;

        match &self.workload {
            TaskWorkload::Embedded(_) => {
                return Err(ChainError::Invalid(format!(
                    "step '{}' uses forbidden Embedded workload",
                    self.name
                )));
            }
            TaskWorkload::Extension(extension) if extension_is_chain(extension) => {
                return Err(ChainError::Invalid(format!(
                    "step '{}' uses forbidden nested {CHAIN_API_VERSION}/{CHAIN_KIND} workload",
                    self.name
                )));
            }
            _ => {}
        }

        self.workload.validate().map_err(|error| {
            ChainError::Invalid(format!("step '{}'.workload is invalid: {error}", self.name))
        })?;
        if let Some(selector) = &self.runner_selector {
            selector.validate().map_err(|error| {
                ChainError::Invalid(format!(
                    "step '{}'.runnerSelector is invalid: {error}",
                    self.name
                ))
            })?;
        }
        Ok(())
    }
}

/// Validated desired state of a chain extension workload.
///
/// Runtime validation covers graph invariants that JSON Schema cannot express:
/// transition targets must exist, every step must be reachable from `entry`, and the directed transition graph must be acyclic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(!try_from, deny_unknown_fields))]
#[serde(rename_all = "camelCase", try_from = "raw::ChainSpecRaw")]
pub struct ChainSpec {
    entry: TaskId,

    #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
    steps: Vec<ChainStep>,
}

impl ChainSpec {
    /// Creates and validates a chain.
    ///
    /// The existing [`TaskId`] validation is used for `entry`, step names, and transition targets.
    /// Names serialize as ordinary JSON strings.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`] when fields, targets, reachability, or acyclicity are invalid.
    pub fn new(entry: impl AsRef<str>, steps: Vec<ChainStep>) -> ChainResult<Self> {
        let spec = Self {
            entry: step_name("entry", entry)?,
            steps,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Entry step name.
    #[inline]
    pub fn entry(&self) -> &TaskId {
        &self.entry
    }

    /// Declared steps in manifest order.
    #[inline]
    pub fn steps(&self) -> &[ChainStep] {
        &self.steps
    }

    /// Finds a declared step by name.
    pub fn step(&self, name: &str) -> Option<&ChainStep> {
        self.steps.iter().find(|step| step.name == name)
    }

    /// Validates all fields and graph invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::Invalid`].
    pub fn validate(&self) -> ChainResult<()> {
        if self.steps.is_empty() {
            return Err(ChainError::Invalid(
                "steps must contain at least one step".to_owned(),
            ));
        }

        self.entry.validate_format().map_err(|error| {
            ChainError::Invalid(format!("entry '{}' is invalid: {error}", self.entry))
        })?;

        let mut indices = HashMap::with_capacity(self.steps.len());
        for (index, step) in self.steps.iter().enumerate() {
            step.validate()?;
            if indices.insert(step.name.as_str(), index).is_some() {
                return Err(ChainError::Invalid(format!(
                    "duplicate step name '{}'",
                    step.name
                )));
            }
        }

        let Some(&entry_index) = indices.get(self.entry.as_str()) else {
            return Err(ChainError::Invalid(format!(
                "entry '{}' does not name a declared step",
                self.entry
            )));
        };

        let mut edges = vec![Vec::with_capacity(2); self.steps.len()];
        let mut indegree = vec![0_usize; self.steps.len()];
        for (source, step) in self.steps.iter().enumerate() {
            if let Some(target) = &step.on_success {
                add_edge(
                    &indices,
                    &mut edges,
                    &mut indegree,
                    source,
                    step,
                    "onSuccess",
                    target,
                )?;
            }
            if let Some(transition) = &step.on_failure {
                add_edge(
                    &indices,
                    &mut edges,
                    &mut indegree,
                    source,
                    step,
                    "onFailure.next",
                    transition.next(),
                )?;
            }
        }

        validate_reachability(&self.steps, &edges, entry_index)?;
        validate_acyclic(&edges, indegree)?;
        Ok(())
    }

    /// Encodes this typed spec as a Solti extension workload.
    ///
    /// # Errors
    ///
    /// Returns an error if validation or JSON conversion fails.
    pub fn into_workload(self) -> ChainResult<TaskWorkload> {
        self.try_into()
    }

    /// Decodes and validates a Solti extension workload as a chain.
    ///
    /// # Errors
    ///
    /// Returns [`ChainError::UnexpectedWorkload`] for a different GVK and a validation or JSON error for an invalid chain `spec`.
    pub fn from_workload(workload: &TaskWorkload) -> ChainResult<Self> {
        workload.try_into()
    }
}

/// Returns whether `workload` is the chain extension GVK owned by this crate.
pub fn is_chain_workload(workload: &TaskWorkload) -> bool {
    matches!(workload, TaskWorkload::Extension(extension) if extension_is_chain(extension))
}

impl TryFrom<ChainSpec> for ExtensionWorkload {
    type Error = ChainError;

    fn try_from(spec: ChainSpec) -> Result<Self, Self::Error> {
        spec.validate()?;
        let value = serde_json::to_value(spec)?;
        Ok(ExtensionWorkload::new(
            CHAIN_API_VERSION,
            CHAIN_KIND,
            value,
        )?)
    }
}

impl TryFrom<ChainSpec> for TaskWorkload {
    type Error = ChainError;

    fn try_from(spec: ChainSpec) -> Result<Self, Self::Error> {
        Ok(Self::Extension(spec.try_into()?))
    }
}

impl TryFrom<&ExtensionWorkload> for ChainSpec {
    type Error = ChainError;

    fn try_from(workload: &ExtensionWorkload) -> Result<Self, Self::Error> {
        if !extension_is_chain(workload) {
            return Err(unexpected_workload(workload.api_version(), workload.kind()));
        }
        Ok(serde_json::from_value(workload.spec().clone())?)
    }
}

impl TryFrom<ExtensionWorkload> for ChainSpec {
    type Error = ChainError;

    fn try_from(workload: ExtensionWorkload) -> Result<Self, Self::Error> {
        (&workload).try_into()
    }
}

impl TryFrom<&TaskWorkload> for ChainSpec {
    type Error = ChainError;

    fn try_from(workload: &TaskWorkload) -> Result<Self, Self::Error> {
        match workload {
            TaskWorkload::Extension(extension) => extension.try_into(),
            _ => Err(unexpected_workload(workload.api_version(), workload.kind())),
        }
    }
}

impl TryFrom<TaskWorkload> for ChainSpec {
    type Error = ChainError;

    fn try_from(workload: TaskWorkload) -> Result<Self, Self::Error> {
        (&workload).try_into()
    }
}

fn extension_is_chain(workload: &ExtensionWorkload) -> bool {
    workload.api_version() == CHAIN_API_VERSION && workload.kind() == CHAIN_KIND
}

fn unexpected_workload(api_version: &str, kind: &str) -> ChainError {
    ChainError::UnexpectedWorkload {
        expected_api_version: CHAIN_API_VERSION,
        expected_kind: CHAIN_KIND,
        api_version: api_version.to_owned(),
        kind: kind.to_owned(),
    }
}

fn step_name(field: &str, value: impl AsRef<str>) -> ChainResult<TaskId> {
    TaskId::new(value).map_err(|error| ChainError::Invalid(format!("{field} is invalid: {error}")))
}

#[allow(clippy::too_many_arguments)]
fn add_edge(
    indices: &HashMap<&str, usize>,
    edges: &mut [Vec<usize>],
    indegree: &mut [usize],
    source: usize,
    step: &ChainStep,
    field: &str,
    target: &TaskId,
) -> ChainResult<()> {
    let Some(&target_index) = indices.get(target.as_str()) else {
        return Err(ChainError::Invalid(format!(
            "step '{}'.{field} target '{}' does not name a declared step",
            step.name, target
        )));
    };
    edges[source].push(target_index);
    indegree[target_index] += 1;
    Ok(())
}

fn validate_reachability(
    steps: &[ChainStep],
    edges: &[Vec<usize>],
    entry: usize,
) -> ChainResult<()> {
    let mut reachable = vec![false; steps.len()];
    let mut pending = vec![entry];
    reachable[entry] = true;
    while let Some(source) = pending.pop() {
        for &target in &edges[source] {
            if !reachable[target] {
                reachable[target] = true;
                pending.push(target);
            }
        }
    }

    let unreachable = steps
        .iter()
        .zip(reachable)
        .filter(|(_, reachable)| !reachable)
        .map(|(step, _)| step.name.as_str())
        .collect::<Vec<_>>();
    if unreachable.is_empty() {
        Ok(())
    } else {
        Err(ChainError::Invalid(format!(
            "steps unreachable from entry: {}",
            unreachable.join(", ")
        )))
    }
}

fn validate_acyclic(edges: &[Vec<usize>], mut indegree: Vec<usize>) -> ChainResult<()> {
    let mut ready = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, &degree)| (degree == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut visited = 0_usize;

    while let Some(source) = ready.pop_front() {
        visited += 1;
        for &target in &edges[source] {
            indegree[target] -= 1;
            if indegree[target] == 0 {
                ready.push_back(target);
            }
        }
    }

    if visited == edges.len() {
        Ok(())
    } else {
        Err(ChainError::Invalid(
            "transition graph must be acyclic".to_owned(),
        ))
    }
}

mod raw {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct ChainStepRaw {
        name: TaskId,
        workload: TaskWorkload,
        #[serde(default)]
        runner_selector: Option<LabelSelector>,
        #[serde(default)]
        on_success: Option<TaskId>,
        #[serde(default)]
        on_failure: Option<FailureTransition>,
    }

    impl TryFrom<ChainStepRaw> for ChainStep {
        type Error = ChainError;

        fn try_from(raw: ChainStepRaw) -> Result<Self, Self::Error> {
            let step = Self {
                name: raw.name,
                workload: raw.workload,
                runner_selector: raw.runner_selector,
                on_success: raw.on_success,
                on_failure: raw.on_failure,
            };
            step.validate()?;
            Ok(step)
        }
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub(super) struct ChainSpecRaw {
        entry: TaskId,
        steps: Vec<ChainStep>,
    }

    impl TryFrom<ChainSpecRaw> for ChainSpec {
        type Error = ChainError;

        fn try_from(raw: ChainSpecRaw) -> Result<Self, Self::Error> {
            let spec = Self {
                entry: raw.entry,
                steps: raw.steps,
            };
            spec.validate()?;
            Ok(spec)
        }
    }
}
