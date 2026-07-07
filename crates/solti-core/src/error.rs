//! # Error types.

use thiserror::Error;

use solti_runner::RunnerError;

/// Error type returned by every fallible operation in solti-core.
///
/// Each variant notes the HTTP status that solti-api maps it to.
/// The enum is `#[non_exhaustive]`: match with a wildcard arm and treat
/// unknown variants as an internal error (`500`).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// A taskvisor runtime failure (submit / cancel / remove / shutdown). → `500`.
    ///
    /// Carries the typed [`taskvisor::Error`] as the source; `op` is a stable
    /// label for the failed operation: `"submit"`, `"cancel"`, `"remove"`, or `"shutdown"`.
    #[error("supervisor {op} failed: {source}")]
    Supervisor {
        /// The operation that failed.
        op: &'static str,
        /// The underlying taskvisor error.
        #[source]
        source: taskvisor::Error,
    },

    /// A task with this name is already active (non-terminal).
    /// Kept distinct from a generic supervisor error; callers map it to `409 Conflict`.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// The referenced task does not exist. Maps to `404 Not Found`.
    #[error("task not found: {0}")]
    NotFound(String),

    /// A model→taskvisor policy mapping failure:
    /// a `#[non_exhaustive]` model enum carried a variant with no taskvisor equivalent. → `500`.
    #[error("mapping error: {0}")]
    Mapping(String),

    /// A runner failed to build the concrete task.
    /// Wraps [`solti_runner::RunnerError`]. → `500`.
    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),

    /// The submitted spec failed validation.
    /// Wraps [`solti_model::ModelError`]. → `400 Bad Request`.
    #[error("invalid spec: {0}")]
    InvalidSpec(#[from] solti_model::ModelError),
}

impl CoreError {
    /// Wrap a taskvisor failure with the operation label.
    ///
    /// Accepts both [`taskvisor::RuntimeError`] and [`taskvisor::ControllerError`]
    /// through the umbrella [`taskvisor::Error`].
    pub(crate) fn supervisor(op: &'static str, e: impl Into<taskvisor::Error>) -> Self {
        Self::Supervisor {
            op,
            source: e.into(),
        }
    }
}
