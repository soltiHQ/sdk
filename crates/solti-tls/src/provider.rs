//! Process-wide rustls `CryptoProvider` install.

/// Install the `ring` crypto provider as the process-wide default if no provider has been set yet.
///
/// This helper:
/// - Checks if a provider is already installed (any kind).
/// - If absent, installs the `ring` provider.
/// - Tolerates a race where another thread installed a provider between our check and our installation.
///
/// Safe to call from multiple places, multiple times.
/// Called automatically by `solti-tls` builders before they hand a config to rustls.
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
