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

    /// The runner could not build the task because of an internal failure.
    #[error("internal error: {0}")]
    Internal(String),
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

    /// The selected runner failed to build the task.
    #[error("runner '{runner}' failed to build task: {source}")]
    Build {
        /// Selected runner name.
        runner: String,
        /// Concrete runner failure.
        #[source]
        source: RunnerError,
    },

    /// The runner returned a task with a name different from the allocated run id.
    #[error(
        "runner '{runner}' returned task name '{actual}', expected allocated run id '{expected}'"
    )]
    RunIdMismatch {
        /// Selected runner name.
        runner: String,
        /// Run id allocated by the router.
        expected: String,
        /// Task name returned by the runner.
        actual: String,
    },
}
