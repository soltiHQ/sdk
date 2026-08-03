//! # Host process errors
//!
//! Backends may preserve this error or map it to their own error contract.

use std::{fmt, io};

/// Error produced while preparing or applying a host process policy.
#[derive(Debug)]
#[non_exhaustive]
pub enum HostProcessError {
    /// The declarative policy is invalid for the selected platform.
    InvalidConfig(String),
    /// An operating-system resource could not be read or prepared.
    Io(io::Error),
}

impl fmt::Display for HostProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for HostProcessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidConfig(_) => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl From<io::Error> for HostProcessError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
