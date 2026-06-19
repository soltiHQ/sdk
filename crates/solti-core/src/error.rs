use thiserror::Error;

use solti_runner::RunnerError;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("supervisor error: {0}")]
    Supervisor(String),

    /// A task with this name is already active (non-terminal). Distinct from a
    /// generic supervisor error so callers can map it to `409 Conflict`.
    #[error("task already exists: {0}")]
    AlreadyExists(String),

    /// The referenced task does not exist. Maps to `404 Not Found`.
    #[error("task not found: {0}")]
    NotFound(String),

    #[error("mapping error: {0}")]
    Mapping(String),

    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),

    #[error("invalid spec: {0}")]
    InvalidSpec(#[from] solti_model::ModelError),
}
