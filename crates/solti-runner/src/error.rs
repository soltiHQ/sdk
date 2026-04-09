//! # Runner errors.
//!
//! [`RunnerError`] covers failures during runner selection and task construction.
//!
//! See [`Runner::build_task`](crate::Runner::build_task) and [`RunnerRouter::build`](crate::RunnerRouter::build)
//! for the primary error sources.

use thiserror::Error;

/// Errors produced by runner operations.
///
/// ## Also
///
/// - [`Runner::build_task`](crate::Runner::build_task) — returns `Result<TaskRef, RunnerError>`.
/// - [`RunnerRouter::build`](crate::RunnerRouter::build) — returns `NoRunner` when no runner matches.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// No registered runner can handle the given task kind.
    #[error("no suitable runner for task kind: {0}")]
    NoRunner(String),

    /// Runner does not support the requested task kind.
    #[error("unsupported task kind for runner '{runner}': {kind}")]
    UnsupportedKind { runner: &'static str, kind: String },

    /// Task specification is invalid for this runner.
    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    /// Internal runner error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Required field is missing from the task spec.
    #[error("missing field: {0}")]
    MissingField(&'static str),

    /// I/O error during task setup.
    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for RunnerError {
    fn from(e: std::io::Error) -> Self {
        RunnerError::Io(e.to_string())
    }
}
