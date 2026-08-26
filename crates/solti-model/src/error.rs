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

impl ModelError {
    /// Returns a stable, low-cardinality label for metrics and structured logs.
    ///
    /// [`Display`](std::fmt::Display) is a human-readable diagnostic and may
    /// include workload data. Use this label for classification.
    pub const fn as_label(&self) -> &'static str {
        match self {
            Self::UnknownAdmission(_) => "unknown_admission",
            Self::UnknownRestart(_) => "unknown_restart",
            Self::UnknownJitter(_) => "unknown_jitter",
            Self::UnknownTaskPhase(_) => "unknown_task_phase",
            Self::Invalid(_) => "invalid",
        }
    }
}

/// Convenience alias for `Result<T, ModelError>`.
pub type ModelResult<T> = Result<T, ModelError>;

#[cfg(test)]
mod tests {
    use super::ModelError;

    #[test]
    fn labels_are_stable_and_low_cardinality() {
        let cases = [
            (
                ModelError::UnknownAdmission("value".into()),
                "unknown_admission",
            ),
            (
                ModelError::UnknownRestart("value".into()),
                "unknown_restart",
            ),
            (ModelError::UnknownJitter("value".into()), "unknown_jitter"),
            (
                ModelError::UnknownTaskPhase("value".into()),
                "unknown_task_phase",
            ),
            (ModelError::Invalid("value".into()), "invalid"),
        ];

        for (error, expected) in cases {
            assert_eq!(error.as_label(), expected);
            assert!(!error.as_label().contains("value"));
        }
    }
}
