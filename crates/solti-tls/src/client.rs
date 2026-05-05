//! Client-side TLS configuration.

use std::path::PathBuf;

use crate::{PemSource, TlsError};

/// Client-side TLS configuration.
///
/// _Construct via [`ClientTlsConfig::builder`]_.
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
    /// Start a new builder.
    pub fn builder() -> ClientTlsConfigBuilder {
        ClientTlsConfigBuilder::default()
    }

    /// Build a [`rustls::ClientConfig`].
    ///
    /// Reads PEM sources, builds a `RootCertStore` from CA, optionally adds the client cert+key for mTLS, and applies ALPN.
    /// All I/O surfaces here.
    ///
    /// Auto-installs the `ring` `CryptoProvider` if no provider is set process-wide.
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
            _ => builder.with_no_client_auth(),
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
    pub fn ca(mut self, src: PemSource) -> Self {
        self.ca = Some(src);
        self
    }

    /// Set the client certificate chain.
    pub fn client_cert(mut self, src: PemSource) -> Self {
        self.client_cert = Some(src);
        self
    }

    /// Set the client private key.
    pub fn client_key(mut self, src: PemSource) -> Self {
        self.client_key = Some(src);
        self
    }

    /// Set the ALPN protocol list, in preference order.
    ///
    /// Pass `["h2"]` for gRPC-only, `["h2", "http/1.1"]` for HTTP (default is empty).
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

    /// Build.
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
