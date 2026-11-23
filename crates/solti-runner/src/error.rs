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

    /// A runner declares a workload type that cannot be used: a refused GVK, a reserved group or
    /// a schema that is not a JSON object or is over 64 KiB.
    #[error(
        "runner '{runner}' declares {api_version}/{kind} with an unusable declaration: {reason}"
    )]
    InvalidWorkloadType {
        /// Runner name.
        runner: String,
        /// Declared workload API group and version, as written.
        api_version: String,
        /// Declared workload kind, as written.
        kind: String,
        /// What is wrong with the declaration.
        reason: String,
    },

    /// A runner's declared capability is invalid, or registering it exceeds an agent-wide cap.
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

    /// The selected runner's declaration refuses the workload body.
    ///
    /// The runner is the first one that matches the GVK and selector, the one that would build the task.
    #[error("workload {api_version}/{kind} rejected by runner '{runner}': {reason}")]
    InvalidWorkload {
        /// Selected runner name.
        runner: String,
        /// Workload API group and version.
        api_version: String,
        /// Workload kind.
        kind: String,
        /// Why the body is refused, without the GVK.
        reason: String,
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
