//! Configuration and registration errors.

use thiserror::Error;

/// Errors returned by runner registration and backend configuration.
///
/// Attempt failures use `taskvisor::TaskError`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExecError {
    /// Runner registration failed.
    #[error(transparent)]
    Router(#[from] solti_runner::RouterError),

    /// Runner identity or backend configuration is invalid.
    #[error("invalid runner configuration: {0}")]
    InvalidRunnerConfig(String),

    /// An operating-system resource could not be prepared.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
