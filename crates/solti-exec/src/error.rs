//! # Execution errors

use thiserror::Error;

/// Errors returned while constructing, registering, and explicitly shutting
/// down execution backends.
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

    /// An operating-system resource or backend lifecycle operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(feature = "host-process")]
impl From<crate::host::HostProcessError> for ExecError {
    fn from(error: crate::host::HostProcessError) -> Self {
        match error {
            crate::host::HostProcessError::InvalidConfig(message) => {
                Self::InvalidRunnerConfig(message)
            }
            crate::host::HostProcessError::Io(error) => Self::Io(error),
        }
    }
}
