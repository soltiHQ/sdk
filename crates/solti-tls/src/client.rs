//! # Client TLS
//!
//! [`ClientTlsConfig`] builds TLS settings for a client.
//! [`LoadedClientTlsConfig`] carries validated PEM for an adapter.
//! It requires server trust roots and accepts an optional client identity.

use crate::{LoadedTlsIdentity, PemRole, TlsError, TlsIdentity, TrustRoots};

/// TLS and mTLS settings for one client.
///
/// [`TrustRoots`] are required.
/// A [`TlsIdentity`] enables client authentication.
///
/// | Configuration                      | Material                                  |
/// |------------------------------------|-------------------------------------------|
/// | [`ClientTlsConfig::new`]           | Server roots; no client certificate       |
/// | [`ClientTlsConfig::with_identity`] | Server roots and one client identity      |
///
/// ## Flow
///
/// ```text
/// TrustRoots ────────────────────┐
///                                ├──► ClientTlsConfig
/// TlsIdentity (optional) ────────┘           │
///                                            ├──► load() ──► LoadedClientTlsConfig
///                                            └──► into_rustls_config() ──► rustls::ClientConfig
/// ```
///
/// ## Rules
///
/// - Constructors do not read files.
/// - [`Self::load`] and [`Self::into_rustls_config`] read and validate every source.
/// - Only [`Self::server_roots`] are trusted; operating-system roots are not added.
/// - The generated configuration has no ALPN protocols; the transport sets them.
/// - `rustls` checks the server name during the handshake.
///
/// ## Example
///
/// ```rust,no_run
/// use solti_tls::{ClientTlsConfig, TlsIdentity, TrustRoots};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let client = ClientTlsConfig::new(TrustRoots::from_pem_file(
///     "/etc/solti/tls/server-ca.crt",
/// ))
/// .with_identity(TlsIdentity::from_pem_files(
///     "/etc/solti/tls/client.crt",
///     "/etc/solti/tls/client.key",
/// ));
///
/// let rustls = client.into_rustls_config()?;
/// assert!(rustls.alpn_protocols.is_empty());
/// # Ok(())
/// # }
/// ```
///
/// ## See Also
///
/// [`ServerTlsConfig`](crate::ServerTlsConfig) configures the server.
#[derive(Clone, Debug)]
pub struct ClientTlsConfig {
    server_roots: TrustRoots,
    identity: Option<TlsIdentity>,
}

impl ClientTlsConfig {
    /// Creates client TLS settings with roots used to verify the server.
    ///
    /// The client presents no certificate until [`Self::with_identity`] is called.
    /// This method only stores the roots.
    pub fn new(server_roots: TrustRoots) -> Self {
        Self {
            server_roots,
            identity: None,
        }
    }

    /// Sets the identity presented when the server requires mutual TLS.
    ///
    /// A second call replaces the previous identity.
    /// This method only stores the identity.
    pub fn with_identity(mut self, identity: TlsIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Returns the roots used to verify the server certificate.
    pub fn server_roots(&self) -> &TrustRoots {
        &self.server_roots
    }

    /// Returns the optional client identity.
    ///
    /// `None` means that the client verifies the server but presents no client certificate.
    pub fn identity(&self) -> Option<&TlsIdentity> {
        self.identity.as_ref()
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
    /// - [`TlsError::Configuration`] when the client certificate and key cannot be used together.
    /// - [`TlsError::InvalidCertificate`] when rustls rejects a trust root.
    ///
    /// # Security
    ///
    /// A loaded client identity contains private-key PEM.
    /// This crate zeroizes it's buffer on drop.
    /// An adapter may keep its own copy.
    pub fn load(self) -> Result<LoadedClientTlsConfig, TlsError> {
        let loaded = self.load_material()?;
        let _ = loaded.rustls_config()?;
        Ok(loaded)
    }

    fn load_material(self) -> Result<LoadedClientTlsConfig, TlsError> {
        Ok(LoadedClientTlsConfig {
            server_roots: self.server_roots.load(PemRole::ServerTrustRoots)?,
            identity: self
                .identity
                .map(|identity| {
                    identity.load(PemRole::ClientCertificate, PemRole::ClientPrivateKey)
                })
                .transpose()?,
        })
    }

    /// Builds a [`rustls::ClientConfig`].
    ///
    /// It trusts only the configured roots.
    /// It presents the client identity when one is set.
    /// The returned `alpn_protocols` list is empty.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::load`].
    ///
    /// # Security
    ///
    /// Pass the expected server name when connecting.
    /// This method does not check it.
    pub fn into_rustls_config(self) -> Result<rustls::ClientConfig, TlsError> {
        self.load_material()?.rustls_config()
    }
}

/// Loaded client PEM accepted by `rustls`.
///
/// `rustls` accepted the server roots.
/// It also accepted the optional certificate chain and key as a client identity.
/// `Debug` redacts the private key through [`LoadedTlsIdentity`].
#[derive(Debug)]
pub struct LoadedClientTlsConfig {
    server_roots: Vec<u8>,
    identity: Option<LoadedTlsIdentity>,
}

impl LoadedClientTlsConfig {
    /// Returns the loaded PEM roots used to verify the server.
    pub fn server_roots_pem(&self) -> &[u8] {
        &self.server_roots
    }

    /// Returns the loaded client identity.
    ///
    /// `None` means that the client presents no certificate.
    pub fn identity(&self) -> Option<&LoadedTlsIdentity> {
        self.identity.as_ref()
    }

    fn rustls_config(&self) -> Result<rustls::ClientConfig, TlsError> {
        crate::provider::ensure_default_provider();

        let certificates =
            crate::pem::load_certificates(&self.server_roots, PemRole::ServerTrustRoots)?;
        let mut roots = rustls::RootCertStore::empty();
        for certificate in certificates {
            roots
                .add(certificate)
                .map_err(|source| TlsError::InvalidCertificate {
                    role: PemRole::ServerTrustRoots,
                    source,
                })?;
        }

        let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
        match &self.identity {
            Some(identity) => {
                let certificates = crate::pem::load_certificates(
                    identity.certificate_chain_pem(),
                    PemRole::ClientCertificate,
                )?;
                let private_key = crate::pem::load_private_key(
                    identity.expose_private_key_pem(),
                    PemRole::ClientPrivateKey,
                )?;
                builder
                    .with_client_auth_cert(certificates, private_key)
                    .map_err(|source| TlsError::Configuration {
                        context: "client identity",
                        source,
                    })
            }
            None => Ok(builder.with_no_client_auth()),
        }
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
    fn client_identity_is_optional() {
        let config = ClientTlsConfig::new(TrustRoots::from_pem_bytes(b"ca"));
        assert!(config.identity().is_none());

        let config = config.with_identity(TlsIdentity::from_pem_bytes(b"cert", b"key"));
        assert!(config.identity().is_some());
    }

    #[test]
    fn load_preserves_material_shape() {
        let (server_ca, _) = self_signed();
        let (certificate, private_key) = self_signed();
        let expected_server_ca = server_ca.clone();
        let expected_certificate = certificate.clone();
        let expected_private_key = private_key.clone();
        let loaded = ClientTlsConfig::new(TrustRoots::from_pem_bytes(server_ca))
            .with_identity(TlsIdentity::from_pem_bytes(certificate, private_key))
            .load()
            .unwrap();
        assert_eq!(loaded.server_roots_pem(), expected_server_ca);
        let identity = loaded.identity().unwrap();
        assert_eq!(identity.certificate_chain_pem(), expected_certificate);
        assert_eq!(identity.expose_private_key_pem(), expected_private_key);
    }

    #[test]
    fn builds_plain_rustls_config_without_transport_alpn() {
        let (ca, _) = self_signed();
        let config = ClientTlsConfig::new(TrustRoots::from_pem_bytes(ca))
            .into_rustls_config()
            .unwrap();
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn rejects_client_certificate_key_mismatch_with_context() {
        let (ca, _) = self_signed();
        let (certificate, _) = self_signed();
        let (_, other_key) = self_signed();
        let error = ClientTlsConfig::new(TrustRoots::from_pem_bytes(ca))
            .with_identity(TlsIdentity::from_pem_bytes(certificate, other_key))
            .load()
            .unwrap_err();
        assert!(matches!(
            error,
            TlsError::Configuration {
                context: "client identity",
                ..
            }
        ));
    }
}
