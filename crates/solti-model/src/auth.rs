//! # Authentication credentials.
//!
//! [`Token`] is a shared bearer secret used in **both** directions of agent ⇄ control-plane communication:
//!   - agent → CP discovery: the agent presents it (see `solti-discover`);
//!   - CP → agent API: the agent verifies inbound calls against it (see `solti-api`).
//!
//! One secret per agent enables auth symmetrically with a single config knob.

use std::fmt;
use std::path::Path;

use subtle::ConstantTimeEq;

use crate::error::{ModelError, ModelResult};

/// A bearer token shared between an agent and the control plane.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wrap a raw token. Surrounding whitespace is trimmed.
    ///
    /// Prefer [`Token::from_env`] / [`Token::from_file`] in production so the secret is not baked into the binary or argv.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into().trim().to_string())
    }

    /// Read the token from an environment variable.
    ///
    /// Returns [`ModelError::Invalid`] if the variable is unset or empty.
    pub fn from_env(var: &str) -> ModelResult<Self> {
        let raw = std::env::var(var)
            .map_err(|_| ModelError::Invalid(format!("token env var `{var}` is not set").into()))?;
        Self::checked(raw)
    }

    /// Read the token from a file (trailing newline / whitespace trimmed).
    ///
    /// Returns [`ModelError::Invalid`] if the file cannot be read or is empty.
    pub fn from_file(path: impl AsRef<Path>) -> ModelResult<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path).map_err(|e| {
            ModelError::Invalid(format!("read token file `{}`: {e}", path.display()).into())
        })?;
        Self::checked(raw)
    }

    fn checked(raw: String) -> ModelResult<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ModelError::Invalid("token must not be empty".into()));
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Borrow the raw token for outbound header construction (the sending side, e.g. `solti-discover`).
    /// Inbound verification should use [`Token::verify`].
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the token is empty (after trimming) — used by config validation.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Constant-time comparison against a candidate presented by a caller.
    ///
    /// Used by the inbound verification path (`solti-api`), a timing side-channel cannot be used to recover the secret byte-by-byte.
    /// A length mismatch returns `false` (token length is not secret);
    /// equal-length content comparison stays constant-time.
    pub fn verify(&self, candidate: &str) -> bool {
        self.0.as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***redacted***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_whitespace() {
        assert_eq!(Token::new("  abc\n").expose(), "abc");
    }

    #[test]
    fn checked_rejects_empty() {
        assert!(Token::from_env("__definitely_unset_var__").is_err());
    }

    #[test]
    fn verify_matches_only_exact() {
        let t = Token::new("s3cr3t-value");
        assert!(t.verify("s3cr3t-value"));
        assert!(!t.verify("s3cr3t-valuE"));
        assert!(!t.verify("s3cr3t"));
        assert!(!t.verify("s3cr3t-value-extra"));
    }

    #[test]
    fn debug_is_redacted() {
        let t = Token::new("super-secret");
        assert_eq!(format!("{t:?}"), "Token(***redacted***)");
        assert!(!format!("{t:?}").contains("super-secret"));
    }
}
