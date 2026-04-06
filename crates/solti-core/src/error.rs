use thiserror::Error;

use solti_runner::RunnerError;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("supervisor error: {0}")]
    Supervisor(String),

    #[error("mapping error: {0}")]
    Mapping(String),

    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),

    #[error("invalid spec: {0}")]
    InvalidSpec(#[from] solti_model::ModelError),
}
