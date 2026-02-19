use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("feature is disabled for kind: {0}")]
    FeatureDisabled(String),

    #[error("unknown admission strategy: {0}")]
    UnknownAdmission(String),

    #[error("unknown restart strategy: {0}")]
    UnknownRestart(String),

    #[error("unknown jitter strategy: {0}")]
    UnknownJitter(String),

    #[error("unknown task kind: {0}")]
    UnknownTaskKind(String),

    #[error("invalid model: {0}")]
    Invalid(String),
}

pub type ModelResult<T> = Result<T, ModelError>;
