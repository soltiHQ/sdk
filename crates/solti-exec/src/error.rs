use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("duplicate runner detected: runner with name '{name}' is already registered")]
    DuplicateRunner { name: String },

    #[error("invalid runner configuration: {0}")]
    InvalidRunnerConfig(String),

    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
