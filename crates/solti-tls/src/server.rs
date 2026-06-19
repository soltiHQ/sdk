//! # Server-side TLS configuration.
//!
//! [`ServerTlsConfig`] (built via [`ServerTlsConfigBuilder`]) describes a TLS listener:
//! the server's own cert/key, an optional client-CA bundle that turns on **mandatory** mTLS, and ALPN.
//! [`ServerTlsConfig::into_rustls_config`] performs the I/O + parsing and yields a [`rustls::ServerConfig`].

use std::path::PathBuf;
use std::sync::Arc;

use crate::{PemSource, TlsError};

/// Server-side TLS configuration.
///
/// Construct via [`ServerTlsConfig::builder`].
///
/// ## Security
///
/// `key` (and `cert`/`client_ca`) are held as [`PemSource`]; the `Bytes` variant keeps the raw private key.
/// The derived `Debug` redacts those bytes (see [`PemSource`]), so logging this struct will not leak the key, but the key is **not** zeroed while the config is alive.
///
/// ## Also
///
/// - [`ClientTlsConfig`](crate::ClientTlsConfig) - the peer side.
/// - [`ServerTlsConfigBuilder`] - the builder.
/// - [`PemSource`], [`TlsError`].
#[derive(Debug, Clone)]
pub struct ServerTlsConfig {
    /// Server certificate chain (leaf first).
    pub cert: PemSource,
    /// Server private key (PKCS#8, PKCS#1, or SEC1).
    pub key: PemSource,
    /// Trusted CA bundle for verifying client certificates (mTLS).
    /// `None` = standard TLS (no client cert required).
    pub client_ca: Option<PemSource>,
    /// ALPN protocol list, in preference order (e.g. `[b"h2"]` for gRPC).
    /// Empty = no ALPN negotiation requested.
    pub alpn: Vec<Vec<u8>>,
}

impl ServerTlsConfig {
    /// Start a new builder.
    pub fn builder() -> ServerTlsConfigBuilder {
        ServerTlsConfigBuilder::default()
    }

    /// Build a [`rustls::ServerConfig`] from this configuration.
    ///
    /// Reads the PEM sources (disk or memory), parses the cert chain and key, optionally constructs a `WebPkiClientVerifier` for mTLS, and applies ALPN.
    /// Auto-installs the `ring` [`CryptoProvider`](crate::ensure_default_provider) if none is set process-wide.
    ///
    /// ## Security
    ///
    /// The server always presents `cert` + `key`.
    /// If `client_ca` is set, client authentication is **mandatory**:
    /// the server demands a client certificate chaining to that CA and rejects unauthenticated clients at the handshake (`WebPkiClientVerifier` defaults to deny-anonymous).
    ///
    /// Server *hostname* is not this method's concern: it is the client that verifies the server's identity.
    ///
    /// ## Errors
    ///
    /// - [`TlsError::Io`]: PEM read
    /// - [`TlsError::NoCertificates`] / [`TlsError::NoPrivateKey`]: parse
    /// - [`TlsError::ClientVerifier`]: mTLS trust-anchor build
    /// - [`TlsError::Rustls`]: e.g. cert/key mismatch
    pub fn into_rustls_config(self) -> Result<rustls::ServerConfig, TlsError> {
        crate::ensure_default_provider();

        let cert_bytes = self.cert.read()?;
        let key_bytes = self.key.read()?;

        let certs = crate::load_certs_from_pem(cert_bytes.as_slice())?;
        let key = crate::load_key_from_pem(key_bytes.as_slice())?;

        let builder = rustls::ServerConfig::builder();
        let server_builder = match self.client_ca {
            Some(ca_src) => {
                let ca_bytes = ca_src.read()?;
                let ca_certs = crate::load_certs_from_pem(ca_bytes.as_slice())?;
                let mut roots = rustls::RootCertStore::empty();
                for ca in ca_certs {
                    roots.add(ca)?;
                }
                let verifier =
                    rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };

        let mut config = server_builder.with_single_cert(certs, key)?;
        config.alpn_protocols = self.alpn;
        Ok(config)
    }
}

/// Incremental builder for [`ServerTlsConfig`].
#[derive(Debug, Default, Clone)]
pub struct ServerTlsConfigBuilder {
    cert: Option<PemSource>,
    key: Option<PemSource>,
    client_ca: Option<PemSource>,
    alpn: Vec<Vec<u8>>,
}

impl ServerTlsConfigBuilder {
    /// Set the server cert chain from any [`PemSource`].
    pub fn cert(mut self, src: PemSource) -> Self {
        self.cert = Some(src);
        self
    }

    /// Set the server private key from any [`PemSource`].
    pub fn key(mut self, src: PemSource) -> Self {
        self.key = Some(src);
        self
    }

    /// Set the ALPN protocol list, in preference order.
    ///
    /// Pass `["h2"]` for gRPC-only, `["h2", "http/1.1"]` for axum HTTP.
    /// Default is empty (no ALPN negotiation).
    pub fn with_alpn<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Vec<u8>>,
    {
        self.alpn = protocols.into_iter().map(Into::into).collect();
        self
    }

    /// Convenience: set the server cert chain from a file path.
    pub fn cert_pem_file(self, path: impl Into<PathBuf>) -> Self {
        self.cert(PemSource::Path(path.into()))
    }

    /// Convenience: set the server cert chain from in-memory bytes.
    pub fn cert_pem_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.cert(PemSource::Bytes(bytes.into()))
    }

    /// Convenience: set the server private key from a file path.
    pub fn key_pem_file(self, path: impl Into<PathBuf>) -> Self {
        self.key(PemSource::Path(path.into()))
    }

    /// Convenience: set the server private key from in-memory bytes.
    pub fn key_pem_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.key(PemSource::Bytes(bytes.into()))
    }

    /// Convenience: enable mTLS with a CA bundle from a file path.
    pub fn require_client_ca_pem_file(self, path: impl Into<PathBuf>) -> Self {
        self.require_client_ca(PemSource::Path(path.into()))
    }

    /// Convenience: enable mTLS with a CA bundle from in-memory bytes.
    pub fn require_client_ca_pem_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.require_client_ca(PemSource::Bytes(bytes.into()))
    }

    /// Require client certificates signed by this CA bundle (turns on mTLS).
    pub fn require_client_ca(mut self, src: PemSource) -> Self {
        self.client_ca = Some(src);
        self
    }

    /// Finalize the configuration.
    ///
    /// Validates that `cert` and `key` are present (else [`TlsError::MissingField`]).
    /// Does no I/O - the PEM sources are read later by [`ServerTlsConfig::into_rustls_config`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::ServerTlsConfig;
    ///
    /// let cfg = ServerTlsConfig::builder()
    ///     .cert_pem_bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec())
    ///     .key_pem_bytes(b"-----BEGIN PRIVATE KEY-----\n...".to_vec())
    ///     .with_alpn(["h2"])
    ///     .build()
    ///     .unwrap();
    /// assert!(cfg.client_ca.is_none()); // standard TLS until require_client_ca
    /// ```
    pub fn build(self) -> Result<ServerTlsConfig, TlsError> {
        let cert = self.cert.ok_or(TlsError::MissingField("cert"))?;
        let key = self.key.ok_or(TlsError::MissingField("key"))?;
        Ok(ServerTlsConfig {
            cert,
            key,
            client_ca: self.client_ca,
            alpn: self.alpn,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PemSource;

    #[test]
    fn debug_of_config_does_not_leak_key_bytes() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![10, 20, 30])
            .key_pem_bytes(vec![201, 202, 203])
            .build()
            .unwrap();
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("201") && !rendered.contains("202"),
            "config Debug must not leak key bytes: {rendered}"
        );
        assert!(
            rendered.contains("redacted"),
            "expected redaction marker: {rendered}"
        );
    }

    #[test]
    fn builder_returns_config_when_cert_and_key_provided() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(b"--FAKE CERT--".to_vec())
            .key_pem_bytes(b"--FAKE KEY--".to_vec())
            .build()
            .unwrap();
        assert!(matches!(cfg.cert, PemSource::Bytes(_)));
        assert!(matches!(cfg.key, PemSource::Bytes(_)));
    }

    #[test]
    fn builder_errors_when_cert_is_missing() {
        let err = ServerTlsConfig::builder()
            .key_pem_bytes(vec![1])
            .build()
            .unwrap_err();
        assert!(matches!(err, TlsError::MissingField("cert")));
    }

    #[test]
    fn builder_errors_when_key_is_missing() {
        let err = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![1])
            .build()
            .unwrap_err();
        assert!(matches!(err, TlsError::MissingField("key")));
    }

    #[test]
    fn cert_pem_file_creates_path_source() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_file("/etc/server.crt")
            .key_pem_bytes(vec![1])
            .build()
            .unwrap();
        assert!(matches!(cfg.cert, PemSource::Path(_)));
    }

    #[test]
    fn client_ca_defaults_to_none() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![1])
            .key_pem_bytes(vec![2])
            .build()
            .unwrap();
        assert!(cfg.client_ca.is_none());
    }

    #[test]
    fn require_client_ca_pem_bytes_enables_mtls() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![1])
            .key_pem_bytes(vec![2])
            .require_client_ca_pem_bytes(b"--FAKE CA--".to_vec())
            .build()
            .unwrap();
        assert!(matches!(cfg.client_ca, Some(PemSource::Bytes(_))));
    }

    #[test]
    fn require_client_ca_pem_file_enables_mtls() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![1])
            .key_pem_bytes(vec![2])
            .require_client_ca_pem_file("/etc/ca.crt")
            .build()
            .unwrap();
        assert!(matches!(cfg.client_ca, Some(PemSource::Path(_))));
    }

    #[test]
    fn alpn_defaults_to_empty() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![1])
            .key_pem_bytes(vec![2])
            .build()
            .unwrap();
        assert!(cfg.alpn.is_empty());
    }

    #[test]
    fn with_alpn_sets_protocols() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(vec![1])
            .key_pem_bytes(vec![2])
            .with_alpn(["h2", "http/1.1"])
            .build()
            .unwrap();
        assert_eq!(cfg.alpn, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }

    fn rcgen_self_signed() -> (Vec<u8>, Vec<u8>) {
        let b = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        (
            b.cert.pem().into_bytes(),
            b.signing_key.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn into_rustls_config_succeeds_with_real_cert_and_key() {
        let (cert, key) = rcgen_self_signed();
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(cert)
            .key_pem_bytes(key)
            .build()
            .unwrap();

        let _rustls = cfg.into_rustls_config().unwrap();
    }

    #[test]
    fn into_rustls_config_succeeds_with_mtls_client_ca() {
        let (cert, key) = rcgen_self_signed();
        let (ca, _) = rcgen_self_signed();
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(cert)
            .key_pem_bytes(key)
            .require_client_ca_pem_bytes(ca)
            .build()
            .unwrap();

        let _rustls = cfg.into_rustls_config().unwrap();
    }

    #[test]
    fn into_rustls_config_propagates_alpn_to_rustls() {
        let (cert, key) = rcgen_self_signed();
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(cert)
            .key_pem_bytes(key)
            .with_alpn(["h2"])
            .build()
            .unwrap();

        let rustls = cfg.into_rustls_config().unwrap();
        assert_eq!(rustls.alpn_protocols, vec![b"h2".to_vec()]);
    }

    #[test]
    fn into_rustls_config_rejects_cert_key_mismatch() {
        let (cert, _) = rcgen_self_signed();
        let (_, other_key) = rcgen_self_signed();
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(cert)
            .key_pem_bytes(other_key)
            .build()
            .unwrap();

        let err = cfg.into_rustls_config().unwrap_err();
        assert!(
            matches!(err, TlsError::Rustls(_)),
            "cert/key mismatch must surface as TlsError::Rustls, got {err:?}"
        );
    }

    #[test]
    fn into_rustls_config_errors_on_malformed_cert_pem() {
        let (_, key) = rcgen_self_signed();
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(b"not a pem".to_vec())
            .key_pem_bytes(key)
            .build()
            .unwrap();

        let err = cfg.into_rustls_config().unwrap_err();
        assert!(
            matches!(err, TlsError::NoCertificates),
            "malformed cert PEM must surface as NoCertificates, got {err:?}"
        );
    }
}
