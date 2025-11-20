use thiserror::Error;

use crate::runner::RunnerError;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("no suitable runner for task kind: {0}")]
    NoRunner(String),

    #[error("supervisor error: {0}")]
    Supervisor(String),

    #[error("mapping error: {0}")]
    Mapping(String),

    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),
}
