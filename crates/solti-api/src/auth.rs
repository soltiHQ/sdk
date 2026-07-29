//! # Bearer Credentials
//!
//! Shared parser for HTTP headers and gRPC metadata.
//! Both transports accept the same `Bearer <token>` value.

/// Extracts a bearer credential.
///
/// The scheme comparison is case-insensitive.
/// The value after the first space is returned without trimming.
/// [`solti_model::Token::verify`] checks that value.
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
