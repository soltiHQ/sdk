//! # Bearer-credential helpers shared by the HTTP and gRPC transports.
//!
//! Both transports gate requests on the same `Authorization: Bearer <token>` shape
//! (HTTP header / gRPC `authorization` metadata). The header parsing and the
//! parsing lives here so the two gates cannot drift apart.

/// Extract the credential from an `Authorization` header / `authorization` metadata value,
/// accepting the scheme case-insensitively.
///
/// The credential is returned verbatim after the first space. It is never trimmed and is
/// matched byte-for-byte by `Token::verify`.
pub(crate) fn bearer_value(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
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
}
