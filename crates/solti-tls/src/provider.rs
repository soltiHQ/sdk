//! # rustls crypto provider
//!
//! This module preserves an existing process-wide `rustls` provider.
//! It installs the ring provider only when no default exists.

pub(crate) fn ensure_default_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent() {
        ensure_default_provider();
        ensure_default_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
