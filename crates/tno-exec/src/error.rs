use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("unsupported task kind: expected {expected}, got {actual}")]
    UnsupportedKind {
        expected: &'static str,
        actual: String,
    },

    #[error("function not found in registry: {0}")]
    FunctionNotFound(String),

    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    #[error("spawn failed: {0}")]
    Spawn(String),

    #[error("process exited with non-zero code: {0}")]
    NonZeroExit(i32),

    #[error("process terminated by signal")]
    Signal,

    #[error("io error: {0}")]
    Io(String),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type ExecResult<T> = Result<T, ExecError>;
