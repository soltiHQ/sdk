//! # Server TLS config.
//!
//! [`ServerTlsConfig`] describes a TLS listener.
//!
//! It stores:
//! - the server certificate chain,
//! - the server private key,
//! - an optional client CA for mTLS,
//! - optional ALPN protocols.
//!
//! The builder only stores intent.
//! PEM files are read later by [`ServerTlsConfig::into_rustls_config`].

use std::path::PathBuf;
use std::sync::Arc;

use crate::{PemSource, TlsError};

/// Server-side TLS configuration.
///
/// Construct via [`ServerTlsConfig::builder`].
///
/// ## Security
///
/// `key` is a [`PemSource`].
/// If it uses [`PemSource::Bytes`], it holds raw private key bytes.
/// `Debug` output is redacted; logging this struct does not print the key.
/// The key bytes are not zeroed on drop.
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
    /// Start a new server TLS builder.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::ServerTlsConfig;
    ///
    /// let cfg = ServerTlsConfig::builder()
    ///     .cert_pem_bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec())
    ///     .key_pem_bytes(b"-----BEGIN PRIVATE KEY-----\n...".to_vec())
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(cfg.client_ca.is_none());
    /// ```
    pub fn builder() -> ServerTlsConfigBuilder {
        ServerTlsConfigBuilder::default()
    }

    /// Build a [`rustls::ServerConfig`] from this configuration.
    ///
    /// Reads the PEM sources, parses the cert chain and key.
    /// Optionally enables mTLS, applies ALPN, and returns a ready `rustls` config.
    ///
    /// It also calls [`ensure_default_provider`](crate::ensure_default_provider);
    /// the `ring` provider is installed if no provider exists yet.
    ///
    /// ## Security
    ///
    /// The server always presents `cert` and `key`.
    /// If `client_ca` is set, client authentication is mandatory.
    /// Clients without a valid certificate are rejected during the TLS handshake.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_tls::ServerTlsConfig;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let rustls_config = ServerTlsConfig::builder()
    ///     .cert_pem_file("/etc/solti/tls/server.crt")
    ///     .key_pem_file("/etc/solti/tls/server.key")
    ///     .with_alpn(["h2"])
    ///     .build()?
    ///     .into_rustls_config()?;
    ///
    /// # let _ = rustls_config;
    /// # Ok(()) }
    /// ```
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
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{PemSource, ServerTlsConfig};
    ///
    /// let cfg = ServerTlsConfig::builder()
    ///     .cert(PemSource::Bytes(b"cert".to_vec()))
    ///     .key(PemSource::Bytes(b"key".to_vec()))
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(matches!(cfg.cert, PemSource::Bytes(_)));
    /// ```
    pub fn cert(mut self, src: PemSource) -> Self {
        self.cert = Some(src);
        self
    }

    /// Set the server private key from any [`PemSource`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{PemSource, ServerTlsConfig};
    ///
    /// let cfg = ServerTlsConfig::builder()
    ///     .cert(PemSource::Bytes(b"cert".to_vec()))
    ///     .key(PemSource::Bytes(b"key".to_vec()))
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(matches!(cfg.key, PemSource::Bytes(_)));
    /// ```
    pub fn key(mut self, src: PemSource) -> Self {
        self.key = Some(src);
        self
    }

    /// Set the ALPN protocol list, in preference order.
    ///
    /// Pass `["h2"]` for gRPC-only, or `["h2", "http/1.1"]` for HTTP/2 and HTTP/1.1. The default is empty.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::ServerTlsConfig;
    ///
    /// let cfg = ServerTlsConfig::builder()
    ///     .cert_pem_bytes(b"cert".to_vec())
    ///     .key_pem_bytes(b"key".to_vec())
    ///     .with_alpn(["h2", "http/1.1"])
    ///     .build()
    ///     .unwrap();
    ///
    /// assert_eq!(cfg.alpn, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    /// ```
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

    /// Require client certificates signed by this CA bundle.
    ///
    /// This turns on mTLS for the server.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{PemSource, ServerTlsConfig};
    ///
    /// let cfg = ServerTlsConfig::builder()
    ///     .cert_pem_bytes(b"cert".to_vec())
    ///     .key_pem_bytes(b"key".to_vec())
    ///     .require_client_ca(PemSource::Bytes(b"ca".to_vec()))
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(cfg.client_ca.is_some());
    /// ```
    pub fn require_client_ca(mut self, src: PemSource) -> Self {
        self.client_ca = Some(src);
        self
    }

    /// Finalize the configuration.
    ///
    /// Validates that `cert` and `key` are present.
    ///
    /// This does no I/O.
    /// PEM files are read later by [`ServerTlsConfig::into_rustls_config`].
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
    /// assert!(cfg.client_ca.is_none()); // standard TLS, not mTLS
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
