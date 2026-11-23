//! # Agent capabilities
//!
//! [`AgentCapabilities`] is an immutable runner capability snapshot.
//! [`RunnerCapability`] describes one registered runner.
//!
//! Runner order is preserved.
//! Workload types inside each runner are stored in canonical GVK order.
//! Each type carries the fingerprint of its schema; the schemas themselves are listed once
//! per fingerprint by [`AgentCapabilities::schemas`].

use std::collections::HashSet;

use serde::Serialize;

use crate::{
    Labels, ModelError, ModelResult, SchemaFingerprint, WorkloadSchema, WorkloadType,
    WorkloadTypeMeta, domain::workload::in_reserved_group, validation,
};

/// Most runners one agent registers.
pub const MAX_AGENT_RUNNERS: usize = 64;

/// Most workload types one runner declares.
pub const MAX_RUNNER_WORKLOAD_TYPES: usize = 256;

/// Most distinct schema fingerprints one agent declares across its runners.
pub const MAX_AGENT_WORKLOAD_SCHEMAS: usize = 256;

/// One registered runner and the workload types it can execute.
///
/// Labels are the same static labels used by `runnerSelector`.
/// A capability is built by the agent, never read back from the wire.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerCapability {
    name: String,
    labels: Labels,
    workload_types: Vec<WorkloadType>,
}

impl RunnerCapability {
    /// Creates a registered runner capability.
    ///
    /// Workload types are stored in canonical GVK order.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the name or labels are invalid, no workload type is
    /// declared, more than [`MAX_RUNNER_WORKLOAD_TYPES`] are, a GVK is duplicated, or a type is in
    /// the reserved `solti.io` group.
    pub fn new(
        name: impl Into<String>,
        labels: Labels,
        mut workload_types: Vec<WorkloadType>,
    ) -> ModelResult<Self> {
        let name = name.into();
        if name.is_empty() {
            return Err(ModelError::Invalid(
                "runner capability name must not be empty".into(),
            ));
        }
        validation::validate_label_value("runner capability name", &name)?;
        labels.validate()?;
        if workload_types.is_empty() {
            return Err(ModelError::Invalid(
                "runner capability must declare at least one workload type".into(),
            ));
        }
        if workload_types.len() > MAX_RUNNER_WORKLOAD_TYPES {
            return Err(ModelError::Invalid(
                format!(
                    "runner capability {name}: {} workload types, over the limit of {MAX_RUNNER_WORKLOAD_TYPES}",
                    workload_types.len()
                )
                .into(),
            ));
        }
        if let Some(reserved) = workload_types
            .iter()
            .find(|workload| in_reserved_group(workload.api_version()))
        {
            return Err(ModelError::Invalid(
                format!(
                    "runner capability {name}: workload {}/{}: {}",
                    reserved.api_version(),
                    reserved.kind(),
                    crate::domain::workload::RESERVED_GROUP_REASON
                )
                .into(),
            ));
        }

        workload_types.sort_by(|left, right| {
            left.api_version()
                .cmp(right.api_version())
                .then_with(|| left.kind().cmp(right.kind()))
        });
        if workload_types
            .windows(2)
            .any(|pair| pair[0].meta() == pair[1].meta())
        {
            return Err(ModelError::Invalid(
                format!("runner capability {name}: duplicate workload GVK").into(),
            ));
        }

        Ok(Self {
            name,
            labels,
            workload_types,
        })
    }

    /// Registered runner name.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Static labels used by `runnerSelector`.
    #[inline]
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Canonically ordered workload types handled by the runner.
    #[inline]
    pub fn workload_types(&self) -> &[WorkloadType] {
        &self.workload_types
    }
}

/// Immutable snapshot of agent execution capabilities.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    runners: Vec<RunnerCapability>,
}

impl AgentCapabilities {
    /// Creates a capability snapshot in runner registration order.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when runner names are duplicated, there are more than
    /// [`MAX_AGENT_RUNNERS`] runners, or the runners declare more than
    /// [`MAX_AGENT_WORKLOAD_SCHEMAS`] distinct schema fingerprints.
    pub fn new(runners: Vec<RunnerCapability>) -> ModelResult<Self> {
        if runners.len() > MAX_AGENT_RUNNERS {
            return Err(ModelError::Invalid(
                format!(
                    "agent capabilities: {} runners, over the limit of {MAX_AGENT_RUNNERS}",
                    runners.len()
                )
                .into(),
            ));
        }
        let mut names = HashSet::with_capacity(runners.len());
        if runners
            .iter()
            .any(|runner| !names.insert(runner.name().to_owned()))
        {
            return Err(ModelError::Invalid(
                "agent capabilities contain a duplicate runner name".into(),
            ));
        }
        let capabilities = Self { runners };
        let distinct = capabilities.schemas().len();
        if distinct > MAX_AGENT_WORKLOAD_SCHEMAS {
            return Err(ModelError::Invalid(
                format!(
                    "agent capabilities: {distinct} distinct workload schemas, over the limit of {MAX_AGENT_WORKLOAD_SCHEMAS}"
                )
                .into(),
            ));
        }
        Ok(capabilities)
    }

    /// Registered runners in routing priority order.
    #[inline]
    pub fn runners(&self) -> &[RunnerCapability] {
        &self.runners
    }

    /// Every distinct schema once, by fingerprint, in first-declaration order.
    pub fn schemas(&self) -> Vec<&WorkloadSchema> {
        let mut seen = HashSet::new();
        self.runners
            .iter()
            .flat_map(|runner| runner.workload_types())
            .filter(|workload| seen.insert(workload.fingerprint()))
            .map(WorkloadType::schema)
            .collect()
    }

    /// GVKs, across all runners in registration order, whose schema has `fingerprint`.
    pub fn types_for(&self, fingerprint: &SchemaFingerprint) -> Vec<&WorkloadTypeMeta> {
        self.runners
            .iter()
            .flat_map(|runner| runner.workload_types())
            .filter(|workload| workload.fingerprint() == fingerprint)
            .map(WorkloadType::meta)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(tag: &str) -> WorkloadSchema {
        WorkloadSchema::from_value(&json!({
            "$schema": crate::WORKLOAD_SCHEMA_DIALECT,
            "type": "object",
            "description": tag,
        }))
        .unwrap()
    }

    fn workload(api_version: &str, kind: &str) -> WorkloadType {
        typed(api_version, kind, "shared")
    }

    fn typed(api_version: &str, kind: &str, tag: &str) -> WorkloadType {
        WorkloadType::new(
            WorkloadTypeMeta::new(api_version, kind).unwrap(),
            schema(tag),
        )
    }

    fn runner(name: &str, types: Vec<WorkloadType>) -> RunnerCapability {
        RunnerCapability::new(name, Labels::new(), types).unwrap()
    }

    #[test]
    fn capability_canonicalizes_workload_types() {
        let capability = RunnerCapability::new(
            "runner-1",
            Labels::new(),
            vec![
                workload("tasks.example.io/v1", "Resize"),
                workload("exec.solti.io/v1", "Subprocess"),
            ],
        )
        .unwrap();

        assert_eq!(
            capability
                .workload_types()
                .iter()
                .map(|workload| (workload.api_version(), workload.kind()))
                .collect::<Vec<_>>(),
            vec![
                ("exec.solti.io/v1", "Subprocess"),
                ("tasks.example.io/v1", "Resize"),
            ]
        );
    }

    #[test]
    fn capability_rejects_invalid_registration_data() {
        let one = || vec![workload("exec.solti.io/v1", "Subprocess")];
        assert!(RunnerCapability::new("", Labels::new(), one()).is_err());
        assert!(RunnerCapability::new("invalid/name", Labels::new(), one()).is_err());
        assert!(RunnerCapability::new("runner", Labels::new(), Vec::new()).is_err());
    }

    #[test]
    fn runner_name_uses_kubernetes_label_value_rules() {
        let capability = |name: String| {
            RunnerCapability::new(
                name,
                Labels::new(),
                vec![workload("exec.solti.io/v1", "Subprocess")],
            )
        };

        assert!(capability("Runner_A.1".into()).is_ok());
        for invalid in [
            String::new(),
            "-runner".into(),
            "runner-".into(),
            "runner/name".into(),
            "r".repeat(64),
        ] {
            assert!(capability(invalid).is_err());
        }
    }

    #[test]
    fn capabilities_reject_duplicate_runner_names() {
        let first = runner("runner", vec![workload("exec.solti.io/v1", "Subprocess")]);
        let second = runner("runner", vec![workload("tasks.example.io/v1", "Resize")]);

        assert!(AgentCapabilities::new(vec![first, second]).is_err());
    }
}
