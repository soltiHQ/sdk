use std::borrow::Cow;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("conflict: resource_version mismatch (expected {expected}, got {actual})")]
    Conflict { expected: u64, actual: u64 },

    #[error("unknown admission policy: {0}")]
    UnknownAdmission(String),

    #[error("unknown restart policy: {0}")]
    UnknownRestart(String),

    #[error("unknown jitter policy: {0}")]
    UnknownJitter(String),

    #[error("unknown task kind: {0}")]
    UnknownTaskKind(String),

    #[error("invalid model: {0}")]
    Invalid(Cow<'static, str>),
}

pub type ModelResult<T> = Result<T, ModelError>;
