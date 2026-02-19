use thiserror::Error;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("unsupported task kind for runner '{runner}': {kind}")]
    UnsupportedKind { runner: &'static str, kind: String },

    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("missing field: {0}")]
    MissingField(&'static str),

    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for RunnerError {
    fn from(e: std::io::Error) -> Self {
        RunnerError::Io(e.to_string())
    }
}
