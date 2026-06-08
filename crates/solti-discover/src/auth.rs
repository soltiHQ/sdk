//! # Agent → control-plane authentication credentials.
//!
//! [`Token`] is a bearer secret the agent presents to the control plane on each sync.

use std::fmt;
use std::path::Path;

use crate::errors::DiscoverError;

/// A bearer token presented to the control plane on each sync.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wrap a raw token.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into().trim().to_string())
    }

    /// Read the token from an environment variable.
    ///
    /// Returns [`DiscoverError::InvalidConfig`] if the variable is unset or empty.
    pub fn from_env(var: &str) -> Result<Self, DiscoverError> {
        let raw = std::env::var(var).map_err(|_| {
            DiscoverError::InvalidConfig(format!("token env var `{var}` is not set"))
        })?;
        Self::checked(raw)
    }

    /// Read the token from a file (trailing newline / whitespace trimmed).
    ///
    /// Returns [`DiscoverError::InvalidConfig`] if the file cannot be read or is empty.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, DiscoverError> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|e| {
            DiscoverError::InvalidConfig(format!("read token file `{}`: {e}", path.display()))
        })?;
        Self::checked(raw)
    }

    fn checked(raw: String) -> Result<Self, DiscoverError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DiscoverError::InvalidConfig(
                "token must not be empty".into(),
            ));
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Borrow the raw token for header construction.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the token is empty (after trimming) - used by config validation.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***redacted***)")
    }
}
