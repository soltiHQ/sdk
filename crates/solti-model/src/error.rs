//! # Model errors.

use std::borrow::Cow;

use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModelError {
    /// Parsing an unrecognized admission-policy name (`AdmissionPolicy` `FromStr`).
    #[error("unknown admission policy: {0}")]
    UnknownAdmission(String),

    /// Parsing an unrecognized restart-policy name (`RestartPolicy` `FromStr`).
    #[error("unknown restart policy: {0}")]
    UnknownRestart(String),

    /// Parsing an unrecognized jitter-policy name (`JitterPolicy` `FromStr`).
    #[error("unknown jitter policy: {0}")]
    UnknownJitter(String),

    /// Parsing an unrecognized phase name (`TaskPhase` `FromStr`).
    #[error("unknown task phase: {0}")]
    UnknownTaskPhase(String),

    /// A structural or business-rule validation failure (empty slot, zero timeout, bad backoff, `Embedded` kind at the submit boundary, malformed identity, …).
    #[error("invalid model: {0}")]
    Invalid(Cow<'static, str>),
}

pub type ModelResult<T> = Result<T, ModelError>;
