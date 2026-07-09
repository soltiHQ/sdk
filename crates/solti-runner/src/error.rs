//! # Runner errors.

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RunnerError {
    /// No registered runner can handle the given task kind.
    #[error("no suitable runner for task kind: {0}")]
    NoRunner(String),

    /// Runner does not support the requested task kind.
    #[error("unsupported task kind for runner '{runner}': {kind}")]
    UnsupportedKind {
        /// Name of the runner that rejected the spec (see [`Runner::name`](crate::Runner::name)).
        runner: &'static str,
        /// Label of the rejected task kind (e.g. `"wasm"`).
        kind: String,
    },

    /// Task specification is invalid for this runner.
    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    /// Internal runner error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Required field is missing from the task spec.
    #[error("missing field: {0}")]
    MissingField(&'static str),

    /// I/O error during task setup (spawn, cgroup, module read, etc.).
    /// The underlying [`std::io::Error`] is preserved as the source. Callers can inspect its [`ErrorKind`](std::io::ErrorKind).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
