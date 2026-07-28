//! # Agent capabilities
//!
//! [`AgentCapabilities`] is an immutable runner capability snapshot.
//! [`RunnerCapability`] describes one registered runner.
//!
//! Runner order is preserved.
//! Workload GVKs inside each runner are stored in canonical order.

use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{Labels, ModelError, ModelResult, WORKLOAD_API_VERSION, WorkloadTypeMeta, validation};

const EMBEDDED_WORKLOAD_KIND: &str = "Embedded";

/// One registered runner and the workload GVKs it can execute.
///
/// Labels are the same static labels used by `runnerSelector`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerCapability {
    name: String,
    labels: Labels,
    workload_types: Vec<WorkloadTypeMeta>,
}

impl<'de> Deserialize<'de> for RunnerCapability {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct RawRunnerCapability {
            name: String,
            #[serde(default)]
            labels: Labels,
            workload_types: Vec<WorkloadTypeMeta>,
        }

        let raw = RawRunnerCapability::deserialize(deserializer)?;
        Self::new(raw.name, raw.labels, raw.workload_types).map_err(serde::de::Error::custom)
    }
}

impl RunnerCapability {
    /// Creates a registered runner capability.
    ///
    /// Workload types are stored in canonical GVK order.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the name or labels are invalid, no workload is declared, a GVK is duplicated, or Embedded is declared.
    pub fn new(
        name: impl Into<String>,
        labels: Labels,
        mut workload_types: Vec<WorkloadTypeMeta>,
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
                "runner capability must declare at least one workload GVK".into(),
            ));
        }
        if workload_types.iter().any(|workload| {
            workload.api_version() == WORKLOAD_API_VERSION
                && workload.kind() == EMBEDDED_WORKLOAD_KIND
        }) {
            return Err(ModelError::Invalid(
                "runner capability must not declare the Embedded workload".into(),
            ));
        }

        workload_types.sort_by(|left, right| {
            left.api_version()
                .cmp(right.api_version())
                .then_with(|| left.kind().cmp(right.kind()))
        });
        if workload_types.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ModelError::Invalid(
                "runner capability contains a duplicate workload GVK".into(),
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

    /// Canonically ordered workload GVKs handled by the runner.
    #[inline]
    pub fn workload_types(&self) -> &[WorkloadTypeMeta] {
        &self.workload_types
    }
}

/// Immutable snapshot of agent execution capabilities.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    runners: Vec<RunnerCapability>,
}

impl<'de> Deserialize<'de> for AgentCapabilities {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct RawAgentCapabilities {
            #[serde(default)]
            runners: Vec<RunnerCapability>,
        }

        let raw = RawAgentCapabilities::deserialize(deserializer)?;
        Self::new(raw.runners).map_err(serde::de::Error::custom)
    }
}

impl AgentCapabilities {
    /// Creates a capability snapshot in runner registration order.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when runner names are duplicated.
    pub fn new(runners: Vec<RunnerCapability>) -> ModelResult<Self> {
        let mut names = HashSet::with_capacity(runners.len());
        if runners
            .iter()
            .any(|runner| !names.insert(runner.name().to_owned()))
        {
            return Err(ModelError::Invalid(
                "agent capabilities contain a duplicate runner name".into(),
            ));
        }
        Ok(Self { runners })
    }

    /// Registered runners in routing priority order.
    #[inline]
    pub fn runners(&self) -> &[RunnerCapability] {
        &self.runners
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workload(api_version: &str, kind: &str) -> WorkloadTypeMeta {
        WorkloadTypeMeta::new(api_version, kind).unwrap()
    }

    #[test]
    fn capability_canonicalizes_workload_types() {
        let capability = RunnerCapability::new(
            "runner-1",
            Labels::new(),
            vec![
                workload("tasks.example.io/v1", "Resize"),
                workload(WORKLOAD_API_VERSION, "Subprocess"),
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
                ("solti.io/v1", "Subprocess"),
                ("tasks.example.io/v1", "Resize"),
            ]
        );
    }

    #[test]
    fn capability_rejects_invalid_registration_data() {
        assert!(
            RunnerCapability::new(
                "",
                Labels::new(),
                vec![workload("solti.io/v1", "Subprocess")]
            )
            .is_err()
        );
        assert!(
            RunnerCapability::new(
                "invalid/name",
                Labels::new(),
                vec![workload("solti.io/v1", "Subprocess")],
            )
            .is_err()
        );
        assert!(RunnerCapability::new("runner", Labels::new(), Vec::new()).is_err());
        assert!(
            RunnerCapability::new(
                "runner",
                Labels::new(),
                vec![
                    workload("solti.io/v1", "Subprocess"),
                    workload("solti.io/v1", "Subprocess"),
                ],
            )
            .is_err()
        );
        assert!(
            RunnerCapability::new(
                "runner",
                Labels::new(),
                vec![workload(WORKLOAD_API_VERSION, EMBEDDED_WORKLOAD_KIND)],
            )
            .is_err()
        );
    }

    #[test]
    fn runner_name_uses_kubernetes_label_value_rules() {
        let capability = |name: String| {
            RunnerCapability::new(
                name,
                Labels::new(),
                vec![workload(WORKLOAD_API_VERSION, "Subprocess")],
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
        let first = RunnerCapability::new(
            "runner",
            Labels::new(),
            vec![workload(WORKLOAD_API_VERSION, "Subprocess")],
        )
        .unwrap();
        let second = RunnerCapability::new(
            "runner",
            Labels::new(),
            vec![workload("tasks.example.io/v1", "Resize")],
        )
        .unwrap();

        assert!(AgentCapabilities::new(vec![first, second]).is_err());
    }

    #[test]
    fn serde_is_strict_and_validated() {
        let valid = serde_json::json!({
            "runners": [{
                "name": "runner",
                "labels": {"zone": "eu"},
                "workloadTypes": [{
                    "apiVersion": "solti.io/v1",
                    "kind": "Subprocess"
                }]
            }]
        });
        let capabilities: AgentCapabilities = serde_json::from_value(valid).unwrap();
        assert_eq!(capabilities.runners()[0].labels().get("zone"), Some("eu"));

        let unknown = serde_json::json!({
            "runners": [],
            "unknown": true
        });
        assert!(serde_json::from_value::<AgentCapabilities>(unknown).is_err());
    }
}
