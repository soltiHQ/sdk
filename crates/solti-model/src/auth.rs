//! Authentication credentials.
//!
//! [`Token`] is a shared bearer secret used in both directions of agent and control-plane communication:
//! - agent to control plane discovery: the agent presents it;
//! - control plane to agent API: the agent verifies it.
//!
//! One secret per agent is enough for both paths.

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

/// Bearer token shared between an agent and the control plane.
///
/// `Debug` output is redacted.
/// Use [`Self::verify`] for inbound checks and [`Self::expose`] only when building an outbound auth header.
///
/// ## Example
///
/// ```
/// use solti_model::Token;
///
/// let token = Token::new("secret");
///
/// assert!(token.verify("secret"));
/// assert!(!token.verify("other"));
/// assert_eq!(format!("{token:?}"), "Token(***redacted***)");
/// ```
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wrap a raw token. Surrounding whitespace is trimmed.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Token;
    ///
    /// let token = Token::new("  secret\n");
    /// assert_eq!(token.expose(), "secret");
    /// ```
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into().trim().to_string())
    }

    /// Generate a fresh random token.
    ///
    /// The token has 256 bits of OS entropy, is base64url-encoded, and starts with `solti_agt_`.
    /// This function does not persist it.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Token;
    ///
    /// let token = Token::generate();
    /// assert!(token.expose().starts_with("solti_agt_"));
    /// assert!(token.verify(token.expose()));
    /// ```
    pub fn generate() -> Self {
        let mut buf = [0u8; GENERATED_ENTROPY_BYTES];
        getrandom::fill(&mut buf).expect("getrandom: OS entropy source unavailable");
        Self(format!("{GENERATED_PREFIX}{}", URL_SAFE_NO_PAD.encode(buf)))
    }

    /// Read the token from an environment variable.
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

    /// Read the token from a file (trailing newline / whitespace trimmed).
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

    /// Borrow the raw token for outbound header construction.
    ///
    /// Inbound verification should use [`Self::verify`].
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Return whether the token is empty after trimming.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Compare with a candidate presented by a caller.
    ///
    /// The comparison is constant-time for equal-length strings.
    /// A length mismatch returns `false`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::Token;
    ///
    /// let token = Token::new("secret");
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

    #[test]
    fn generate_is_prefixed_unique_and_self_verifying() {
        let a = Token::generate();
        let b = Token::generate();
        assert!(a.expose().starts_with("solti_agt_"));
        assert_ne!(a.expose(), b.expose(), "two generated tokens must differ");
        assert!(a.verify(a.expose()));
        assert!(!a.verify(b.expose()));
        assert_eq!(a.expose().len(), "solti_agt_".len() + 43);
    }
}
