//! # Model errors
//!
//! [`ModelError`] reports parsing and validation failures.

use std::borrow::Cow;

use thiserror::Error;

/// Error type for parsing and validating model values.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModelError {
    /// Unknown admission policy name.
    #[error("unknown admission policy: {0}")]
    UnknownAdmission(String),

    /// Unknown restart policy name.
    #[error("unknown restart policy: {0}")]
    UnknownRestart(String),

    /// Unknown jitter policy name.
    #[error("unknown jitter policy: {0}")]
    UnknownJitter(String),

    /// Unknown task phase name.
    #[error("unknown task phase: {0}")]
    UnknownTaskPhase(String),

    /// Invalid model value or state.
    #[error("invalid model: {0}")]
    Invalid(Cow<'static, str>),
}

/// Convenience alias for `Result<T, ModelError>`.
pub type ModelResult<T> = Result<T, ModelError>;
