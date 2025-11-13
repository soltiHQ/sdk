use thiserror::Error;

#[derive(Error, Debug)]
pub enum ExecError {
    #[error("unsupported kind for this runner")]
    UnsupportedKind,
    #[error("non-zero exit code: {code}")]
    NonZeroExit { code: i32 },
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("killed by signal")]
    KilledBySignal,
    #[error("missing program")]
    MissingProgram,
    #[error("io error: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<std::io::Error> for ExecError {
    fn from(e: std::io::Error) -> Self {
        ExecError::Io(e.to_string())
    }
}
