//! # Trust roots
//!
//! [`TrustRoots`] holds the certificates accepted as peer trust anchors.
//! The same type verifies servers and mTLS clients.

use crate::{PemRole, PemSource, TlsError};

/// A PEM bundle of peer trust anchors.
///
/// ## Use
///
/// | Configuration                                                                         | Verified peer      |
/// |---------------------------------------------------------------------------------------|--------------------|
/// | [`ClientTlsConfig`](crate::ClientTlsConfig)                                           | Server certificate |
/// | [`ServerTlsConfig::require_client_auth`](crate::ServerTlsConfig::require_client_auth) | Client certificate |
///
/// ## Rules
///
/// - Construction does not read or parse the bundle.
/// - The client or server configuration reads and validates it.
/// - The bundle must contain at least one certificate PEM block.
///
/// ## Example
///
/// ```
/// use solti_tls::{ClientTlsConfig, TrustRoots};
///
/// let roots = TrustRoots::from_pem_file("/etc/solti/tls/server-ca.crt");
/// let client = ClientTlsConfig::new(roots);
/// assert!(format!("{:?}", client.server_roots()).contains("server-ca.crt"));
/// ```
#[derive(Clone, Debug)]
pub struct TrustRoots(PemSource);

impl TrustRoots {
    /// Creates trust roots from an explicit [`PemSource`].
    pub fn new(source: PemSource) -> Self {
        Self(source)
    }

    /// Creates trust roots from a PEM file path.
    ///
    /// This method only stores the path.
    pub fn from_pem_file(path: impl Into<std::path::PathBuf>) -> Self {
        Self(PemSource::file(path))
    }

    /// Creates trust roots from in-memory PEM bytes.
    ///
    /// Clones share the same immutable buffer.
    pub fn from_pem_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(PemSource::bytes(bytes))
    }

    /// Returns the PEM source.
    pub fn source(&self) -> &PemSource {
        &self.0
    }

    pub(crate) fn load(self, role: PemRole) -> Result<Vec<u8>, TlsError> {
        self.0.load(role)
    }
}
