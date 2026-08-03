//! # Bearer token
//!
//! [`Token`] wraps one bearer secret.
//! It can be created, generated, or loaded from an environment variable or file.
//!
//! This module does not choose an authentication topology.
//! It does not persist or rotate secrets.

use std::fmt;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use subtle::ConstantTimeEq;

use crate::error::{ModelError, ModelResult};

/// Prefix on generated tokens.
const GENERATED_PREFIX: &str = "solti_agt_";

/// Entropy of a generated token, in bytes (256 bits).
const GENERATED_ENTROPY_BYTES: usize = 32;

/// Validated bearer token.
///
/// `Debug` output is redacted.
/// Use [`Self::verify`] for inbound checks.
/// Use [`Self::expose`] only when the raw value is required.
///
/// ## Example
///
/// ```
/// use solti_model::Token;
///
/// let token = Token::new("secret").unwrap();
///
/// assert!(token.verify("secret"));
/// assert!(!token.verify("other"));
/// assert_eq!(format!("{token:?}"), "Token(***redacted***)");
/// ```
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wraps a raw token.
    ///
    /// Surrounding whitespace is trimmed.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the trimmed value is empty.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Token;
    ///
    /// let token = Token::new("  secret\n").unwrap();
    /// assert_eq!(token.expose(), "secret");
    /// ```
    pub fn new(token: impl Into<String>) -> ModelResult<Self> {
        Self::checked(token.into())
    }

    /// Generates a random token.
    ///
    /// The token uses 256 bits from the operating system entropy source.
    /// It uses unpadded base64url and starts with `solti_agt_`.
    /// The token is not persisted.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the entropy source is unavailable.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Token;
    ///
    /// let token = Token::generate()?;
    /// assert!(token.expose().starts_with("solti_agt_"));
    /// assert!(token.verify(token.expose()));
    /// # Ok::<(), solti_model::ModelError>(())
    /// ```
    pub fn generate() -> ModelResult<Self> {
        let mut buf = [0u8; GENERATED_ENTROPY_BYTES];
        getrandom::fill(&mut buf).map_err(|error| {
            ModelError::Invalid(format!("OS entropy source unavailable: {error}").into())
        })?;
        Ok(Self(format!(
            "{GENERATED_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(buf)
        )))
    }

    /// Reads a token from an environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the variable is absent or the value is empty.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_model::Token;
    ///
    /// let token = Token::from_env("SOLTI_AGENT_TOKEN")?;
    /// # Ok::<(), solti_model::ModelError>(())
    /// ```
    pub fn from_env(var: &str) -> ModelResult<Self> {
        let raw = std::env::var(var)
            .map_err(|_| ModelError::Invalid(format!("token env var `{var}` is not set").into()))?;
        Self::checked(raw)
    }

    /// Reads a token from a UTF-8 file.
    ///
    /// Surrounding whitespace is trimmed.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the file cannot be read or the value is empty.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_model::Token;
    ///
    /// let token = Token::from_file("/run/secrets/solti-agent-token")?;
    /// # Ok::<(), solti_model::ModelError>(())
    /// ```
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

    /// Returns the raw token.
    ///
    /// Inbound verification should use [`Self::verify`].
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Verifies a candidate value.
    ///
    /// The comparison is constant-time for equal-length strings.
    /// A length mismatch returns `false`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Token;
    ///
    /// let token = Token::new("secret").unwrap();
    /// assert!(token.verify("secret"));
    /// assert!(!token.verify("Secret"));
    /// ```
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
        assert_eq!(Token::new("  abc\n").unwrap().expose(), "abc");
    }

    #[test]
    fn checked_rejects_empty() {
        assert!(Token::new("").is_err());
        assert!(Token::new(" \t\n").is_err());
        assert!(Token::from_env("__definitely_unset_var__").is_err());
    }

    #[test]
    fn verify_matches_only_exact() {
        let t = Token::new("s3cr3t-value").unwrap();
        assert!(t.verify("s3cr3t-value"));
        assert!(!t.verify("s3cr3t-valuE"));
        assert!(!t.verify("s3cr3t"));
        assert!(!t.verify("s3cr3t-value-extra"));
    }

    #[test]
    fn debug_is_redacted() {
        let t = Token::new("super-secret").unwrap();
        assert_eq!(format!("{t:?}"), "Token(***redacted***)");
        assert!(!format!("{t:?}").contains("super-secret"));
    }

    #[test]
    fn generate_is_prefixed_unique_and_self_verifying() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert!(a.expose().starts_with("solti_agt_"));
        assert_ne!(a.expose(), b.expose(), "two generated tokens must differ");
        assert!(a.verify(a.expose()));
        assert!(!a.verify(b.expose()));
        assert_eq!(a.expose().len(), "solti_agt_".len() + 43);
    }
}
