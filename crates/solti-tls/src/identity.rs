//! # TLS identities
//!
//! [`TlsIdentity`] keeps one certificate chain with one private key.
//! [`LoadedTlsIdentity`] exposes the loaded PEM after configuration validation.

use zeroize::Zeroizing;

use crate::{PemRole, PemSource, PrivateKeySource, TlsError};

/// One certificate chain and its private key.
///
/// ## Flow
///
/// ```text
/// PemSource ─────────────┐
///                        ├──► TlsIdentity ──► client or server config
/// PrivateKeySource ──────┘
/// ```
///
/// ## Rules
///
/// - The certificate chain is passed to `rustls` in its original order; put the end-entity certificate first, follow it with the issuer chain.
/// - File sources are read when a client or server configuration is loaded or built.
/// - Every source is validated at that boundary.
/// - An identity always contains both sources.
/// - [`PrivateKeySource`] zeroizes its in-memory bytes after the last owner is dropped.
/// - `Debug` does not expose in-memory PEM.
///
/// ## Example
///
/// ```
/// use solti_tls::TlsIdentity;
///
/// let identity = TlsIdentity::from_pem_files(
///     "/etc/solti/tls/server.crt",
///     "/etc/solti/tls/server.key",
/// );
/// assert!(format!("{:?}", identity.private_key()).contains("server.key"));
/// ```
///
/// ## See Also
///
/// - [`ServerTlsConfig`](crate::ServerTlsConfig) requires a server identity.
/// - [`ClientTlsConfig::with_identity`](crate::ClientTlsConfig::with_identity) adds a client identity.
#[derive(Clone, Debug)]
pub struct TlsIdentity {
    certificate_chain: PemSource,
    private_key: PrivateKeySource,
}

impl TlsIdentity {
    /// Creates an identity from separate certificate and private-key sources.
    ///
    /// This method only stores the sources.
    pub fn new(certificate_chain: PemSource, private_key: PrivateKeySource) -> Self {
        Self {
            certificate_chain,
            private_key,
        }
    }

    /// Creates an identity from certificate-chain and private-key PEM paths.
    ///
    /// This method does not read the files.
    pub fn from_pem_files(
        certificate_chain: impl Into<std::path::PathBuf>,
        private_key: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self::new(
            PemSource::file(certificate_chain),
            PrivateKeySource::file(private_key),
        )
    }

    /// Creates an identity from certificate-chain and private-key PEM bytes.
    ///
    /// The private-key bytes move into zeroizing storage.
    pub fn from_pem_bytes(
        certificate_chain: impl Into<Vec<u8>>,
        private_key: impl Into<Vec<u8>>,
    ) -> Self {
        Self::new(
            PemSource::bytes(certificate_chain),
            PrivateKeySource::bytes(private_key),
        )
    }

    /// Returns the certificate-chain source.
    pub fn certificate_chain(&self) -> &PemSource {
        &self.certificate_chain
    }

    /// Returns the private-key source.
    pub fn private_key(&self) -> &PrivateKeySource {
        &self.private_key
    }

    pub(crate) fn load(
        self,
        certificate_role: PemRole,
        private_key_role: PemRole,
    ) -> Result<LoadedTlsIdentity, TlsError> {
        Ok(LoadedTlsIdentity {
            certificate_chain: self.certificate_chain.load(certificate_role)?,
            private_key: self.private_key.load(private_key_role)?,
        })
    }
}

/// Loaded identity PEM accepted as part of a client or server configuration.
///
/// [`ServerTlsConfig::load`](crate::ServerTlsConfig::load) and
/// [`ClientTlsConfig::load`](crate::ClientTlsConfig::load) produce this value.
/// `rustls` accepted the certificate chain and private key together.
/// The value keeps PEM encoding for adapters that do not use `rustls` types.
pub struct LoadedTlsIdentity {
    certificate_chain: Vec<u8>,
    private_key: Zeroizing<Vec<u8>>,
}

impl LoadedTlsIdentity {
    /// Returns the loaded certificate-chain PEM.
    pub fn certificate_chain_pem(&self) -> &[u8] {
        &self.certificate_chain
    }

    /// Exposes the loaded private-key PEM.
    ///
    /// The method name makes private-key access visible at the call site.
    /// The returned slice points to zeroizing storage owned by this identity.
    /// A caller may keep its own copy.
    pub fn expose_private_key_pem(&self) -> &[u8] {
        &self.private_key
    }
}

impl std::fmt::Debug for LoadedTlsIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedTlsIdentity")
            .field(
                "certificate_chain",
                &format_args!("{} bytes", self.certificate_chain.len()),
            )
            .field("private_key", &"[redacted]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_identity_debug_redacts_key() {
        let identity = TlsIdentity::from_pem_bytes(b"cert".to_vec(), b"secret-key".to_vec());
        let loaded = identity
            .load(PemRole::ServerCertificate, PemRole::ServerPrivateKey)
            .unwrap();
        let rendered = format!("{loaded:?}");
        assert!(!rendered.contains("secret-key"));
        assert!(rendered.contains("redacted"));
    }
}
