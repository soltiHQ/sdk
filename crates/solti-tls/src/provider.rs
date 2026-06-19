//! # Process-wide rustls `CryptoProvider` install.

/// Install the `ring` crypto provider as the process-wide default if none is set.
///
/// This helper:
/// - checks whether a provider is already installed (of any kind);
/// - if absent, installs the `ring` provider;
/// - tolerates a race where another thread installed one between the check and the installation (the `install_default` `Err` is ignored).
///
/// Idempotent and safe to call from multiple places, multiple times.
/// Both [`ServerTlsConfig::into_rustls_config`](crate::ServerTlsConfig::into_rustls_config) and [`ClientTlsConfig::into_rustls_config`](crate::ClientTlsConfig::into_rustls_config) call it for you.
///
/// ## Provider policy
///
/// This installs **`ring`** specifically.
/// The default is not configurable: a caller who needs `aws-lc-rs` must install their own provider **before** the first `solti-tls` builder runs (since this becomes a no-op once any provider exists).
/// Protocol versions and cipher suites use `rustls` - safe defaults (the workspace enables TLS 1.2 + 1.3); there is no min-version policy here.
pub fn ensure_default_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Ignore Err: that just means a concurrent caller installed a provider first.
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
