//! # Error types.

use thiserror::Error;

use solti_runner::RunnerError;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// A taskvisor runtime failure (submit / cancel / remove / shutdown). → `500`.
    #[error("supervisor error: {0}")]
    Supervisor(String),

    /// A task with this name is already active (non-terminal). Distinct from a generic supervisor error so callers can map it to `409 Conflict`.
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
