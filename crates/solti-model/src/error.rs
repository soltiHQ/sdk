//! # Model errors.
//!
//! [`ModelError`] covers validation and consistency failures in the domain model.

use std::borrow::Cow;

use thiserror::Error;

/// Errors produced by domain model validation and construction.
///
/// ## Also
///
/// - [`TaskSpec::validate`](crate::TaskSpec::validate) — submit-boundary validation.
/// - [`TaskSpecBuilder::build`](crate::TaskSpecBuilder::build) — builder-time validation.
/// - [`BackoffPolicy::validate`](crate::BackoffPolicy::validate) — backoff parameter validation.
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("unknown admission policy: {0}")]
    UnknownAdmission(String),

    #[error("unknown restart policy: {0}")]
    UnknownRestart(String),

    #[error("unknown jitter policy: {0}")]
    UnknownJitter(String),

    #[error("unknown task phase: {0}")]
    UnknownTaskPhase(String),

    #[error("invalid model: {0}")]
    Invalid(Cow<'static, str>),
}

pub type ModelResult<T> = Result<T, ModelError>;
