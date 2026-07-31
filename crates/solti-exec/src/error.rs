//! # Execution errors
//!
//! [`ExecError`] covers runner construction and registration.
//! Workload build errors use [`solti_runner::RunnerError`].
//! Attempt failures use [`taskvisor::TaskError`].

use thiserror::Error;

/// Errors returned by runner registration and backend configuration.
///
/// Match with a wildcard arm because this enum is non-exhaustive.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExecError {
    /// The runner router rejected registration.
    #[error(transparent)]
    Router(#[from] solti_runner::RouterError),

    /// The runner name or backend configuration is invalid.
    #[error("invalid runner configuration: {0}")]
    InvalidRunnerConfig(String),

    /// An operating-system resource could not be read or prepared.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
