//! # Runner and router errors
//!
//! [`RunnerError`] belongs to one runner implementation.
//! [`RouterError`] belongs to registration, selection, and task construction.

use solti_model::ModelError;
use thiserror::Error;

/// Error returned by a concrete runner while it builds a task.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunnerError {
    /// The runner received a workload GVK it does not implement.
    #[error("unsupported workload for runner '{runner}': {api_version}/{kind}")]
    UnsupportedWorkload {
        /// Runner name.
        runner: String,
        /// Workload API group and version.
        api_version: String,
        /// Workload kind.
        kind: String,
    },

    /// The workload desired state is invalid for this runner.
    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    /// Build cancellation won during interruptible runner-owned work.
    #[error("runner build was cancelled")]
    BuildCancelled,

    /// A composing runner could not build one nested workload.
    ///
    /// The original router error remains available as the error source. This
    /// preserves selection, admission, recursion, cancellation, and concrete
    /// runner failure taxonomy across a composition boundary.
    #[error("{context} could not be built: {source}")]
    NestedBuild {
        /// Composition-specific location of the nested workload.
        context: String,
        /// Original nested router failure.
        #[source]
        source: Box<RouterError>,
    },

    /// The runner could not build the task because of an internal failure.
    #[error("internal error: {0}")]
    Internal(String),
}

impl RunnerError {
    /// Returns a stable, low-cardinality label for metrics and structured logs.
    ///
    /// [`Display`](std::fmt::Display) is a human-readable diagnostic and may
    /// include workload data. Use this label for classification.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::UnsupportedWorkload { .. } => "unsupported_workload",
            Self::InvalidSpec(_) => "invalid_spec",
            Self::BuildCancelled => "build_cancelled",
            Self::NestedBuild { .. } => "nested_build",
            Self::Internal(_) => "internal",
        }
    }
}

/// Error returned by [`RunnerRouter`](crate::RunnerRouter).
///
/// Registration errors preserve the rejected runner name.
/// Build errors preserve the selected runner error as their source.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RouterError {
    /// A runner with this name is already registered.
    #[error("runner '{name}' is already registered")]
    DuplicateRunner {
        /// Duplicate runner name.
        name: String,
    },

    /// Runner labels violate the model's Kubernetes label rules.
    #[error("invalid labels for runner '{runner}': {source}")]
    InvalidLabels {
        /// Runner name.
        runner: String,
        /// Label validation failure.
        #[source]
        source: ModelError,
    },

    /// A runner's declared capability is invalid.
    #[error("invalid capability for runner '{runner}': {source}")]
    InvalidCapability {
        /// Runner name.
        runner: String,
        /// Capability validation failure.
        #[source]
        source: ModelError,
    },

    /// Embedded workloads already carry a prebuilt task and are not routable.
    #[error("embedded workload is not routable")]
    EmbeddedWorkload,

    /// No registered runner matches the workload GVK and selector.
    #[error("no runner matches {api_version}/{kind}")]
    NoRunner {
        /// Workload API group and version.
        api_version: String,
        /// Workload kind.
        kind: String,
    },

    /// Cancellation won while a selected runner waited for build admission.
    #[error("runner '{runner}' build admission was cancelled")]
    BuildCancelled {
        /// Selected runner name.
        runner: String,
    },

    /// A composing runner selected a runner already active in this build path.
    #[error("recursive build through runner '{runner}' is not allowed")]
    RecursiveBuild {
        /// Re-entered runner name.
        runner: String,
    },

    /// A nested admission wait would deadlock with other active root builds.
    #[error("runner '{runner}' build admission would create a wait cycle")]
    AdmissionCycle {
        /// Selected runner name.
        runner: String,
    },

    /// The selected runner failed to build the task.
    #[error("runner '{runner}' failed to build task: {source}")]
    Build {
        /// Selected runner name.
        runner: String,
        /// Concrete runner failure.
        #[source]
        source: RunnerError,
    },
}

impl RouterError {
    /// Returns a stable, low-cardinality label for metrics and structured logs.
    ///
    /// [`Display`](std::fmt::Display) is a human-readable diagnostic and may
    /// include workload data. Use this label for classification.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::DuplicateRunner { .. } => "duplicate_runner",
            Self::InvalidLabels { .. } => "invalid_labels",
            Self::InvalidCapability { .. } => "invalid_capability",
            Self::EmbeddedWorkload => "embedded_workload",
            Self::NoRunner { .. } => "no_runner",
            Self::BuildCancelled { .. } => "build_cancelled",
            Self::RecursiveBuild { .. } => "recursive_build",
            Self::AdmissionCycle { .. } => "admission_cycle",
            Self::Build { .. } => "build",
        }
    }
}

#[cfg(test)]
mod tests {
    use solti_model::ModelError;

    use super::{RouterError, RunnerError};

    #[test]
    fn runner_labels_are_stable_and_low_cardinality() {
        let nested = RouterError::NoRunner {
            api_version: "example.io/v1".into(),
            kind: "Example".into(),
        };
        let cases = [
            (
                RunnerError::UnsupportedWorkload {
                    runner: "runner".into(),
                    api_version: "example.io/v1".into(),
                    kind: "Example".into(),
                },
                "unsupported_workload",
            ),
            (RunnerError::InvalidSpec("value".into()), "invalid_spec"),
            (RunnerError::BuildCancelled, "build_cancelled"),
            (
                RunnerError::NestedBuild {
                    context: "nested".into(),
                    source: Box::new(nested),
                },
                "nested_build",
            ),
            (RunnerError::Internal("value".into()), "internal"),
        ];

        for (error, expected) in cases {
            assert_eq!(error.as_label(), expected);
        }
    }

    #[test]
    fn router_labels_are_stable_and_low_cardinality() {
        let cases = [
            (
                RouterError::DuplicateRunner {
                    name: "runner".into(),
                },
                "duplicate_runner",
            ),
            (
                RouterError::InvalidLabels {
                    runner: "runner".into(),
                    source: ModelError::Invalid("labels".into()),
                },
                "invalid_labels",
            ),
            (
                RouterError::InvalidCapability {
                    runner: "runner".into(),
                    source: ModelError::Invalid("capability".into()),
                },
                "invalid_capability",
            ),
            (RouterError::EmbeddedWorkload, "embedded_workload"),
            (
                RouterError::NoRunner {
                    api_version: "example.io/v1".into(),
                    kind: "Example".into(),
                },
                "no_runner",
            ),
            (
                RouterError::BuildCancelled {
                    runner: "runner".into(),
                },
                "build_cancelled",
            ),
            (
                RouterError::RecursiveBuild {
                    runner: "runner".into(),
                },
                "recursive_build",
            ),
            (
                RouterError::AdmissionCycle {
                    runner: "runner".into(),
                },
                "admission_cycle",
            ),
            (
                RouterError::Build {
                    runner: "runner".into(),
                    source: RunnerError::Internal("value".into()),
                },
                "build",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.as_label(), expected);
        }
    }
}
