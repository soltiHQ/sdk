use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("unsupported task kind: expected {expected}, got {actual}")]
    UnsupportedKind {
        expected: &'static str,
        actual: String,
    },

    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    #[error("invalid runner configuration: {0}")]
    InvalidRunnerConfig(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("duplicate runner-tag detected: runner with tag '{tag}' is already registered")]
    DuplicateRunnerTag { tag: String },
}
