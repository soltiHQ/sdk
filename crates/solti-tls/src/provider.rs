//! # Process-wide rustls provider install.

/// Install the `ring` crypto provider as the process-wide default if none is set.
///
/// This helper:
/// - ignores the harmless race where another thread installs a provider first,
/// - checks whether any provider is already installed,
/// - installs the `ring` provider if none exists.
///
/// ## Provider policy
///
/// This installs `ring` specifically.
/// The default is not configurable:
/// a caller who needs `aws-lc-rs` must install their own provider before the first `solti-tls` builder runs
/// (since this becomes a no-op once any provider exists).
/// Protocol versions and cipher suites use the `rustls` safe defaults (the workspace enables TLS 1.2 + 1.3);
/// there is no min-version policy here.
///
/// ## Example
///
/// ```
/// solti_tls::ensure_default_provider();
/// assert!(rustls::crypto::CryptoProvider::get_default().is_some());
/// ```
pub fn ensure_default_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_default_provider_installs_when_absent() {
        ensure_default_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn ensure_default_provider_is_idempotent() {
        ensure_default_provider();
        ensure_default_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
