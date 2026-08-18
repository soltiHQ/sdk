//! # Server TLS
//!
//! [`ServerTlsConfig`] builds TLS settings for a server.
//! It requires a server identity and accepts optional client trust roots.
//! [`LoadedServerTlsConfig`] carries validated PEM for an adapter.

use std::sync::Arc;

use crate::{LoadedTlsIdentity, PemRole, TlsError, TlsIdentity, TrustRoots};

/// TLS and mTLS settings for one server.
///
/// A [`TlsIdentity`] is required.
/// [`TrustRoots`] enable client authentication.
///
/// | Configuration                            | Material                                |
/// |------------------------------------------|-----------------------------------------|
/// | [`ServerTlsConfig::new`]                 | Server identity; no client roots        |
/// | [`ServerTlsConfig::require_client_auth`] | Server identity and client trust roots  |
///
/// ## Flow
///
/// ```text
/// TlsIdentity ───────────────────┐
///                                ├──► ServerTlsConfig
/// TrustRoots (optional) ─────────┘           │
///                                            ├──► load() ──► LoadedServerTlsConfig
///                                            └──► into_rustls_config() ──► rustls::ServerConfig
/// ```
///
/// ## Rules
///
/// - Constructors do not read files.
/// - [`Self::load`] and [`Self::into_rustls_config`] read and validate every source.
/// - Without client roots, the server does not request a client certificate.
/// - With client roots, every client must present a trusted certificate.
/// - The generated configuration has no ALPN protocols. The transport sets them.
///
/// ## Example
///
/// ```rust,no_run
/// use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let server = ServerTlsConfig::new(TlsIdentity::from_pem_files(
///     "/etc/solti/tls/server.crt",
///     "/etc/solti/tls/server.key",
/// ))
/// .require_client_auth(TrustRoots::from_pem_file(
///     "/etc/solti/tls/client-ca.crt",
/// ));
///
/// let rustls = server.into_rustls_config()?;
/// assert!(rustls.alpn_protocols.is_empty());
/// # Ok(())
/// # }
/// ```
///
/// ## See Also
///
/// [`ClientTlsConfig`](crate::ClientTlsConfig) configures the client.
#[derive(Clone, Debug)]
pub struct ServerTlsConfig {
    identity: TlsIdentity,
    client_auth_roots: Option<TrustRoots>,
}

impl ServerTlsConfig {
    /// Creates server TLS settings with the identity presented to clients.
    ///
    /// Client authentication is disabled until [`Self::require_client_auth`] is called.
    /// This method only stores the identity.
    pub fn new(identity: TlsIdentity) -> Self {
        Self {
            identity,
            client_auth_roots: None,
        }
    }

    /// Requires clients accepted by these trust roots.
    ///
    /// A client must present a certificate that rustls accepts.
    /// A second call replaces the previous roots.
    /// This method only stores the roots.
    pub fn require_client_auth(mut self, roots: TrustRoots) -> Self {
        self.client_auth_roots = Some(roots);
        self
    }

    /// Returns the identity presented by the server.
    pub fn identity(&self) -> &TlsIdentity {
        &self.identity
    }

    /// Returns the roots used to verify client certificates.
    ///
    /// `None` means that client authentication is disabled.
    pub fn client_auth_roots(&self) -> Option<&TrustRoots> {
        self.client_auth_roots.as_ref()
    }

    /// Loads and validates PEM for a transport adapter.
    ///
    /// This method builds a temporary `rustls` configuration to validate the material.
    /// It returns the loaded PEM after that check succeeds.
    /// File errors include the purpose of the PEM and its path.
    ///
    /// # Errors
    ///
    /// - [`TlsError::ReadPem`] when a file cannot be read.
    /// - [`TlsError::InvalidPem`] when a PEM block is malformed.
    /// - [`TlsError::NoCertificates`] when an input has no certificate block.
    /// - [`TlsError::NoPrivateKey`] when the identity has no supported private-key block.
    /// - [`TlsError::MultiplePrivateKeys`] when the identity has more than one key.
    /// - [`TlsError::InvalidCertificate`] when rustls rejects a trust root.
    /// - [`TlsError::ClientVerifier`] when the client verifier cannot be built.
    /// - [`TlsError::Configuration`] when the server certificate and key cannot be used together.
    ///
    /// # Security
    ///
    /// The loaded server identity contains private-key PEM.
    /// This crate zeroizes it's buffer on drop.
    /// An adapter may keep its own copy.
    pub fn load(self) -> Result<LoadedServerTlsConfig, TlsError> {
        let loaded = self.load_material()?;
        let _ = loaded.rustls_config()?;
        Ok(loaded)
    }

    fn load_material(self) -> Result<LoadedServerTlsConfig, TlsError> {
        Ok(LoadedServerTlsConfig {
            identity: self
                .identity
                .load(PemRole::ServerCertificate, PemRole::ServerPrivateKey)?,
            client_auth_roots: self
                .client_auth_roots
                .map(|roots| roots.load(PemRole::ClientTrustRoots))
                .transpose()?,
        })
    }

    /// Builds a [`rustls::ServerConfig`].
    ///
    /// Client-authentication roots make client certificates mandatory.
    /// Without the roots, no client certificate is requested.
    /// The returned `alpn_protocols` list is empty.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`].
    pub fn into_rustls_config(self) -> Result<rustls::ServerConfig, TlsError> {
        self.load_material()?.rustls_config()
    }
}

/// Loaded server PEM accepted by `rustls`.
///
/// `rustls` accepted the certificate chain and key as a server identity.
/// If client roots are present, the client verifier was also built successfully.
/// `Debug` redacts the private key through [`LoadedTlsIdentity`].
#[derive(Debug)]
pub struct LoadedServerTlsConfig {
    identity: LoadedTlsIdentity,
    client_auth_roots: Option<Vec<u8>>,
}

impl LoadedServerTlsConfig {
    /// Returns the loaded server identity.
    pub fn identity(&self) -> &LoadedTlsIdentity {
        &self.identity
    }

    /// Returns the loaded PEM roots used to verify client certificates.
    ///
    /// `None` means that client authentication is disabled.
    pub fn client_auth_roots_pem(&self) -> Option<&[u8]> {
        self.client_auth_roots.as_deref()
    }

    fn rustls_config(&self) -> Result<rustls::ServerConfig, TlsError> {
        crate::provider::ensure_default_provider();

        let certificates = crate::pem::load_certificates(
            self.identity.certificate_chain_pem(),
            PemRole::ServerCertificate,
        )?;
        let private_key = crate::pem::load_private_key(
            self.identity.expose_private_key_pem(),
            PemRole::ServerPrivateKey,
        )?;

        let builder = rustls::ServerConfig::builder();
        let builder = match &self.client_auth_roots {
            Some(client_auth_roots) => {
                let certificates =
                    crate::pem::load_certificates(client_auth_roots, PemRole::ClientTrustRoots)?;
                let mut roots = rustls::RootCertStore::empty();
                for certificate in certificates {
                    roots
                        .add(certificate)
                        .map_err(|source| TlsError::InvalidCertificate {
                            role: PemRole::ClientTrustRoots,
                            source,
                        })?;
                }
                let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .map_err(|source| TlsError::ClientVerifier { source })?;
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };

        builder
            .with_single_cert(certificates, private_key)
            .map_err(|source| TlsError::Configuration {
                context: "server identity",
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed() -> (Vec<u8>, Vec<u8>) {
        let bundle = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        (
            bundle.cert.pem().into_bytes(),
            bundle.signing_key.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn client_auth_is_optional() {
        let config = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(b"cert", b"key"));
        assert!(config.client_auth_roots().is_none());

        let config = config.require_client_auth(TrustRoots::from_pem_bytes(b"ca"));
        assert!(config.client_auth_roots().is_some());
    }

    #[test]
    fn load_preserves_material_shape() {
        let (certificate, private_key) = self_signed();
        let (client_ca, _) = self_signed();
        let expected_certificate = certificate.clone();
        let expected_private_key = private_key.clone();
        let expected_client_ca = client_ca.clone();
        let loaded = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(certificate, private_key))
            .require_client_auth(TrustRoots::from_pem_bytes(client_ca))
            .load()
            .unwrap();
        assert_eq!(
            loaded.identity().certificate_chain_pem(),
            expected_certificate
        );
        assert_eq!(
            loaded.identity().expose_private_key_pem(),
            expected_private_key
        );
        assert_eq!(
            loaded.client_auth_roots_pem(),
            Some(expected_client_ca.as_slice())
        );
    }

    #[test]
    fn builds_plain_rustls_config_without_transport_alpn() {
        let (certificate, private_key) = self_signed();
        let config = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(certificate, private_key))
            .into_rustls_config()
            .unwrap();
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn rejects_certificate_key_mismatch_with_context() {
        let (certificate, _) = self_signed();
        let (_, other_key) = self_signed();
        let error = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(certificate, other_key))
            .into_rustls_config()
            .unwrap_err();
        assert!(matches!(
            error,
            TlsError::Configuration {
                context: "server identity",
                ..
            }
        ));
    }
}
