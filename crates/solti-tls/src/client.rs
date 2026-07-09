//! # Client TLS config.
//!
//! [`ClientTlsConfig`] describes a TLS client.
//!
//! It stores:
//! - optional ALPN protocols,
//! - the CA bundle used to verify the server,
//! - an optional client certificate and key for mTLS.
//!
//! Hostname verification happens when the client connects, not when this config is built.
//! See [`ClientTlsConfig::into_rustls_config`].

use std::path::PathBuf;

use crate::{PemSource, TlsError};

/// Client-side TLS configuration.
///
/// Construct via [`ClientTlsConfig::builder`].
/// `client_cert` and `client_key` are a pair: set both for mTLS, or neither for plain client TLS.
///
/// ## Security
///
/// `client_key` is a [`PemSource`].
/// If it uses [`PemSource::Bytes`], it holds raw private key bytes.
/// `Debug` output is redacted, but the bytes are not zeroed on drop.
///
/// ## Also
///
/// - [`ServerTlsConfig`](crate::ServerTlsConfig) - the peer side.
/// - [`ClientTlsConfigBuilder`] - the builder.
/// - [`PemSource`], [`TlsError`].
#[derive(Debug, Clone)]
pub struct ClientTlsConfig {
    /// Trusted CA bundle for verifying the server's certificate.
    pub ca: PemSource,
    /// Client certificate chain (`None` = no client cert).
    pub client_cert: Option<PemSource>,
    /// Client private key.
    pub client_key: Option<PemSource>,
    /// ALPN protocol list, in preference order.
    pub alpn: Vec<Vec<u8>>,
}

impl ClientTlsConfig {
    /// Start a new client TLS builder.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::ClientTlsConfig;
    ///
    /// let cfg = ClientTlsConfig::builder()
    ///     .ca_pem_bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec())
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(cfg.client_cert.is_none());
    /// ```
    pub fn builder() -> ClientTlsConfigBuilder {
        ClientTlsConfigBuilder::default()
    }

    /// Build a [`rustls::ClientConfig`].
    ///
    /// Reads the PEM sources, builds a root store from `ca`, optionally adds the client cert and key for mTLS.
    /// Applies ALPN, and returns a ready `rustls` config.
    ///
    /// It also calls [`ensure_default_provider`](crate::ensure_default_provider); the `ring` provider is installed if no provider exists yet.
    ///
    /// ## Security: read this!
    ///
    /// The resulting config verifies that the server certificate chains to the `ca` bundle you supplied.
    /// Trust roots come only from your PEM, not from the OS store.
    ///
    /// It does not check the server hostname here.
    /// SAN and identity matching happen when you connect, against the server name you pass to `rustls`, `tonic`, or `reqwest`.
    ///
    /// Pass the real server name.
    /// Do not install a `dangerous()` certificate verifier on the returned config.
    /// Revocation (OCSP/CRL) is not checked.
    ///
    /// If `client_cert` + `client_key` are set, they are presented for mTLS.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// use solti_tls::ClientTlsConfig;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let rustls_config = ClientTlsConfig::builder()
    ///     .ca_pem_file("/etc/solti/tls/control-plane-ca.crt")
    ///     .with_alpn(["h2"])
    ///     .build()?
    ///     .into_rustls_config()?;
    ///
    /// # let _ = rustls_config;
    /// # Ok(()) }
    /// ```
    pub fn into_rustls_config(self) -> Result<rustls::ClientConfig, TlsError> {
        crate::ensure_default_provider();

        let ca_bytes = self.ca.read()?;
        let ca_certs = crate::load_certs_from_pem(ca_bytes.as_slice())?;
        let mut roots = rustls::RootCertStore::empty();
        for ca in ca_certs {
            roots.add(ca)?;
        }

        let builder = rustls::ClientConfig::builder().with_root_certificates(roots);

        let mut config = match (self.client_cert, self.client_key) {
            (Some(cert_src), Some(key_src)) => {
                let cert_bytes = cert_src.read()?;
                let key_bytes = key_src.read()?;
                let certs = crate::load_certs_from_pem(cert_bytes.as_slice())?;
                let key = crate::load_key_from_pem(key_bytes.as_slice())?;
                builder.with_client_auth_cert(certs, key)?
            }
            (None, None) => builder.with_no_client_auth(),
            (Some(_), None) => return Err(TlsError::MissingField("client_key")),
            (None, Some(_)) => return Err(TlsError::MissingField("client_cert")),
        };

        config.alpn_protocols = self.alpn;
        Ok(config)
    }
}

/// Incremental builder for [`ClientTlsConfig`].
#[derive(Debug, Default, Clone)]
pub struct ClientTlsConfigBuilder {
    client_cert: Option<PemSource>,
    client_key: Option<PemSource>,
    ca: Option<PemSource>,
    alpn: Vec<Vec<u8>>,
}

impl ClientTlsConfigBuilder {
    /// Set the trusted CA bundle (verifies the server's certificate).
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{ClientTlsConfig, PemSource};
    ///
    /// let cfg = ClientTlsConfig::builder()
    ///     .ca(PemSource::Bytes(b"ca".to_vec()))
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(matches!(cfg.ca, PemSource::Bytes(_)));
    /// ```
    pub fn ca(mut self, src: PemSource) -> Self {
        self.ca = Some(src);
        self
    }

    /// Set the client certificate chain.
    ///
    /// Set this together with [`client_key`](Self::client_key).
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{ClientTlsConfig, PemSource};
    ///
    /// let cfg = ClientTlsConfig::builder()
    ///     .ca(PemSource::Bytes(b"ca".to_vec()))
    ///     .client_cert(PemSource::Bytes(b"cert".to_vec()))
    ///     .client_key(PemSource::Bytes(b"key".to_vec()))
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(cfg.client_cert.is_some());
    /// ```
    pub fn client_cert(mut self, src: PemSource) -> Self {
        self.client_cert = Some(src);
        self
    }

    /// Set the client private key.
    ///
    /// Set this together with [`client_cert`](Self::client_cert).
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{ClientTlsConfig, PemSource};
    ///
    /// let cfg = ClientTlsConfig::builder()
    ///     .ca(PemSource::Bytes(b"ca".to_vec()))
    ///     .client_cert(PemSource::Bytes(b"cert".to_vec()))
    ///     .client_key(PemSource::Bytes(b"key".to_vec()))
    ///     .build()
    ///     .unwrap();
    ///
    /// assert!(cfg.client_key.is_some());
    /// ```
    pub fn client_key(mut self, src: PemSource) -> Self {
        self.client_key = Some(src);
        self
    }

    /// Set the ALPN protocol list, in preference order.
    ///
    /// Pass `["h2"]` for gRPC-only, or `["h2", "http/1.1"]` for HTTP/2 and HTTP/1.1.
    /// The default is empty.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::ClientTlsConfig;
    ///
    /// let cfg = ClientTlsConfig::builder()
    ///     .ca_pem_bytes(b"ca".to_vec())
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

    /// Convenience: trusted CA bundle from a file path.
    pub fn ca_pem_file(self, path: impl Into<PathBuf>) -> Self {
        self.ca(PemSource::Path(path.into()))
    }

    /// Convenience: trusted CA bundle from in-memory bytes.
    pub fn ca_pem_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.ca(PemSource::Bytes(bytes.into()))
    }

    /// Convenience: client cert chain from a file path.
    pub fn client_cert_pem_file(self, path: impl Into<PathBuf>) -> Self {
        self.client_cert(PemSource::Path(path.into()))
    }

    /// Convenience: client cert chain from in-memory bytes.
    pub fn client_cert_pem_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.client_cert(PemSource::Bytes(bytes.into()))
    }

    /// Convenience: client private key from a file path.
    pub fn client_key_pem_file(self, path: impl Into<PathBuf>) -> Self {
        self.client_key(PemSource::Path(path.into()))
    }

    /// Convenience: client private key from in-memory bytes.
    pub fn client_key_pem_bytes(self, bytes: impl Into<Vec<u8>>) -> Self {
        self.client_key(PemSource::Bytes(bytes.into()))
    }

    /// Finalize the configuration.
    ///
    /// Requires `ca`.
    /// The client cert and key must be set together.
    /// This does no I/O; PEM files are read later by [`ClientTlsConfig::into_rustls_config`].
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::{ClientTlsConfig, TlsError};
    ///
    /// // A client cert without its key is rejected.
    /// let err = ClientTlsConfig::builder()
    ///     .ca_pem_bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec())
    ///     .client_cert_pem_bytes(b"cert".to_vec())
    ///     .build()
    ///     .unwrap_err();
    /// assert!(matches!(err, TlsError::MissingField("client_key")));
    /// ```
    pub fn build(self) -> Result<ClientTlsConfig, TlsError> {
        let ca = self.ca.ok_or(TlsError::MissingField("ca"))?;
        match (&self.client_cert, &self.client_key) {
            (Some(_), None) => return Err(TlsError::MissingField("client_key")),
            (None, Some(_)) => return Err(TlsError::MissingField("client_cert")),
            _ => {}
        }
        Ok(ClientTlsConfig {
            ca,
            client_cert: self.client_cert,
            client_key: self.client_key,
            alpn: self.alpn,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PemSource;

    #[test]
    fn builder_returns_config_with_ca() {
        let cfg = ClientTlsConfig::builder()
            .ca_pem_bytes(b"--FAKE CA--".to_vec())
            .build()
            .unwrap();
        assert!(matches!(cfg.ca, PemSource::Bytes(_)));
        assert!(cfg.client_cert.is_none());
        assert!(cfg.client_key.is_none());
        assert!(cfg.alpn.is_empty());
    }

    #[test]
    fn builder_errors_when_ca_is_missing() {
        let err = ClientTlsConfig::builder().build().unwrap_err();
        assert!(matches!(err, TlsError::MissingField("ca")));
    }

    #[test]
    fn with_client_cert_pair_enables_mtls() {
        let cfg = ClientTlsConfig::builder()
            .ca_pem_bytes(vec![1])
            .client_cert_pem_bytes(b"cert".to_vec())
            .client_key_pem_bytes(b"key".to_vec())
            .build()
            .unwrap();
        assert!(matches!(cfg.client_cert, Some(PemSource::Bytes(_))));
        assert!(matches!(cfg.client_key, Some(PemSource::Bytes(_))));
    }

    #[test]
    fn builder_errors_when_client_cert_without_key() {
        let err = ClientTlsConfig::builder()
            .ca_pem_bytes(vec![1])
            .client_cert_pem_bytes(b"cert".to_vec())
            .build()
            .unwrap_err();
        assert!(matches!(err, TlsError::MissingField("client_key")));
    }

    #[test]
    fn builder_errors_when_client_key_without_cert() {
        let err = ClientTlsConfig::builder()
            .ca_pem_bytes(vec![1])
            .client_key_pem_bytes(b"key".to_vec())
            .build()
            .unwrap_err();
        assert!(matches!(err, TlsError::MissingField("client_cert")));
    }

    #[test]
    fn with_alpn_sets_protocols() {
        let cfg = ClientTlsConfig::builder()
            .ca_pem_bytes(vec![1])
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
    fn into_rustls_config_succeeds_with_ca_only() {
        let (ca, _) = rcgen_self_signed();
        let cfg = ClientTlsConfig::builder().ca_pem_bytes(ca).build().unwrap();
        let _rustls = cfg.into_rustls_config().unwrap();
    }

    #[test]
    fn into_rustls_config_succeeds_with_mtls_client_cert() {
        let (ca, _) = rcgen_self_signed();
        let (cert, key) = rcgen_self_signed();
        let cfg = ClientTlsConfig::builder()
            .ca_pem_bytes(ca)
            .client_cert_pem_bytes(cert)
            .client_key_pem_bytes(key)
            .build()
            .unwrap();
        let _rustls = cfg.into_rustls_config().unwrap();
    }

    #[test]
    fn into_rustls_config_succeeds_when_constructed_directly_without_mtls() {
        let (ca, _) = rcgen_self_signed();
        let cfg = ClientTlsConfig {
            ca: PemSource::Bytes(ca),
            client_cert: None,
            client_key: None,
            alpn: Vec::new(),
        };
        let _rustls = cfg.into_rustls_config().unwrap();
    }

    #[test]
    fn into_rustls_config_rejects_direct_cert_without_key() {
        let (ca, _) = rcgen_self_signed();
        let (cert, _) = rcgen_self_signed();
        let cfg = ClientTlsConfig {
            ca: PemSource::Bytes(ca),
            client_cert: Some(PemSource::Bytes(cert)),
            client_key: None,
            alpn: Vec::new(),
        };
        let err = cfg.into_rustls_config().unwrap_err();
        assert!(
            matches!(err, TlsError::MissingField("client_key")),
            "unpaired client_cert must not silently disable mTLS, got {err:?}"
        );
    }

    #[test]
    fn into_rustls_config_rejects_direct_key_without_cert() {
        let (ca, _) = rcgen_self_signed();
        let (_, key) = rcgen_self_signed();
        let cfg = ClientTlsConfig {
            ca: PemSource::Bytes(ca),
            client_cert: None,
            client_key: Some(PemSource::Bytes(key)),
            alpn: Vec::new(),
        };
        let err = cfg.into_rustls_config().unwrap_err();
        assert!(
            matches!(err, TlsError::MissingField("client_cert")),
            "unpaired client_key must not silently disable mTLS, got {err:?}"
        );
    }

    #[test]
    fn into_rustls_config_propagates_alpn_to_rustls() {
        let (ca, _) = rcgen_self_signed();
        let cfg = ClientTlsConfig::builder()
            .ca_pem_bytes(ca)
            .with_alpn(["h2"])
            .build()
            .unwrap();
        let rustls = cfg.into_rustls_config().unwrap();
        assert_eq!(rustls.alpn_protocols, vec![b"h2".to_vec()]);
    }
}
