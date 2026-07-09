//! # Bearer-credential helpers shared by the HTTP and gRPC transports.
//!
//! Both transports gate requests on the same `Authorization: Bearer <token>` shape
//! (HTTP header / gRPC `authorization` metadata). The header parsing and the
//! construction-time token check live here so the two gates cannot drift apart.

use solti_model::Token;

/// Extract the credential from an `Authorization` header / `authorization` metadata value,
/// accepting the scheme case-insensitively.
///
/// The credential is returned verbatim after the first space. It is never trimmed and is
/// matched byte-for-byte by [`Token::verify`].
pub(crate) fn bearer_value(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

/// Reject an empty token at auth-gate construction time.
///
/// An empty shared secret would make the gate accept `Authorization: Bearer ` (an empty
/// credential), silently disabling authentication. Panicking turns that misconfiguration
/// into a loud startup failure — mirroring `solti-discover`, which rejects an empty token
/// when its config is built.
pub(crate) fn assert_auth_token_not_empty(token: &Token) {
    assert!(
        !token.is_empty(),
        "auth token must not be empty: an empty token would accept an empty bearer credential"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_value_accepts_scheme_case_insensitively() {
        assert_eq!(bearer_value("Bearer tok"), Some("tok"));
        assert_eq!(bearer_value("bearer tok"), Some("tok"));
        assert_eq!(bearer_value("BEARER tok"), Some("tok"));
        assert_eq!(bearer_value("BeArEr tok"), Some("tok"));
        assert_eq!(bearer_value("Bearer a b"), Some("a b"));
        assert_eq!(bearer_value("Basic tok"), None);
        assert_eq!(bearer_value("tok"), None);
        assert_eq!(bearer_value(""), None);
    }

    #[test]
    fn assert_auth_token_not_empty_accepts_non_empty() {
        assert_auth_token_not_empty(&Token::new("tok"));
    }

    #[test]
    #[should_panic(expected = "auth token must not be empty")]
    fn assert_auth_token_not_empty_panics_on_empty() {
        // `Token::new` trims, so a whitespace-only token is empty too.
        assert_auth_token_not_empty(&Token::new("   "));
    }
}
