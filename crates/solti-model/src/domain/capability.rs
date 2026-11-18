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
    fn runner_capability_serializes_schema_fingerprints() {
        let capability = RunnerCapability::new(
            "runner",
            {
                let mut labels = Labels::new();
                labels.insert("zone", "eu");
                labels
            },
            vec![typed("tasks.example.io/v1", "Resize", "resize")],
        )
        .unwrap();
        let fingerprint = capability.workload_types()[0]
            .fingerprint()
            .as_str()
            .to_owned();

        assert_eq!(
            serde_json::to_value(&capability).unwrap(),
            json!({
                "name": "runner",
                "labels": {"zone": "eu"},
                "workloadTypes": [{
                    "apiVersion": "tasks.example.io/v1",
                    "kind": "Resize",
                    "schemaFingerprint": fingerprint,
                }]
            })
        );
    }

    #[test]
    fn runner_capability_refuses_duplicates_reserved_group_and_over_256_types() {
        // The same GVK twice is a duplicate even when the schemas differ.
        let duplicate = RunnerCapability::new(
            "runner",
            Labels::new(),
            vec![
                typed("tasks.example.io/v1", "Resize", "a"),
                typed("tasks.example.io/v1", "Resize", "b"),
            ],
        );
        assert!(duplicate.unwrap_err().to_string().contains("duplicate"));

        for (api_version, kind) in [("solti.io/v1", "Embedded"), ("solti.io/v1", "Other")] {
            let error =
                RunnerCapability::new("runner", Labels::new(), vec![workload(api_version, kind)])
                    .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("API group solti.io is reserved for SDK-internal workloads"),
                "{error}"
            );
        }

        let many = |count: usize| {
            (0..count)
                .map(|i| workload("tasks.example.io/v1", &format!("Kind{i}")))
                .collect::<Vec<_>>()
        };
        assert!(RunnerCapability::new("runner", Labels::new(), many(256)).is_ok());
        let error = RunnerCapability::new("runner", Labels::new(), many(257)).unwrap_err();
        assert!(error.to_string().contains("256"), "{error}");
    }

    #[test]
    fn capabilities_reject_duplicate_runner_names() {
        let first = runner("runner", vec![workload("exec.solti.io/v1", "Subprocess")]);
        let second = runner("runner", vec![workload("tasks.example.io/v1", "Resize")]);

        assert!(AgentCapabilities::new(vec![first, second]).is_err());
    }

    #[test]
    fn agent_capabilities_refuse_over_64_runners_and_over_256_distinct_fingerprints() {
        let runners = |count: usize| {
            (0..count)
                .map(|i| {
                    runner(
                        &format!("runner-{i}"),
                        vec![workload("a.example.io/v1", "A")],
                    )
                })
                .collect::<Vec<_>>()
        };
        assert!(AgentCapabilities::new(runners(64)).is_ok());
        let error = AgentCapabilities::new(runners(65)).unwrap_err();
        assert!(error.to_string().contains("64"), "{error}");

        // 256 distinct schemas across two runners are fine; the 257th is refused.
        let split = |total: usize| {
            let types = |range: std::ops::Range<usize>| {
                range
                    .map(|i| typed("tasks.example.io/v1", &format!("Kind{i}"), &format!("s{i}")))
                    .collect::<Vec<_>>()
            };
            vec![
                runner("first", types(0..200)),
                runner("second", types(200..total)),
            ]
        };
        assert!(AgentCapabilities::new(split(256)).is_ok());
        let error = AgentCapabilities::new(split(257)).unwrap_err();
        assert!(error.to_string().contains("256"), "{error}");

        // The same schema declared by many runners counts once.
        let shared = (0..64)
            .map(|i| {
                runner(
                    &format!("runner-{i}"),
                    (0..8)
                        .map(|k| workload("tasks.example.io/v1", &format!("Kind{k}")))
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        assert!(AgentCapabilities::new(shared).is_ok());
    }

    #[test]
    fn agent_capabilities_list_a_shared_schema_once() {
        let capabilities = AgentCapabilities::new(vec![
            runner(
                "first",
                vec![
                    typed("tasks.example.io/v1", "Alpha", "shared"),
                    typed("tasks.example.io/v1", "Beta", "own"),
                ],
            ),
            runner(
                "second",
                vec![typed("other.example.io/v1", "Gamma", "shared")],
            ),
        ])
        .unwrap();

        let schemas = capabilities.schemas();
        assert_eq!(schemas.len(), 2);
        assert_eq!(
            schemas[0].fingerprint(),
            &schema("shared").fingerprint().clone()
        );
        assert_eq!(schemas[1].fingerprint(), schema("own").fingerprint());

        let shared = capabilities.types_for(schemas[0].fingerprint());
        assert_eq!(
            shared
                .iter()
                .map(|meta| (meta.api_version(), meta.kind()))
                .collect::<Vec<_>>(),
            vec![
                ("tasks.example.io/v1", "Alpha"),
                ("other.example.io/v1", "Gamma")
            ]
        );
        assert_eq!(capabilities.types_for(schemas[1].fingerprint()).len(), 1);
    }
}
