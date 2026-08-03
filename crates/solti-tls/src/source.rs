//! # PEM sources
//!
//! [`PemSource`] holds certificates and trust roots.
//! [`PrivateKeySource`] holds private keys in zeroizing storage.
//! Both can use a file path or in-memory bytes.
//!
//! ## Flow
//!
//! ```text
//! file path ──► read during load ──┐
//!                                  ├──► PEM bytes
//! memory bytes ────────────────────┘
//! ```
//!
//! Constructors do not parse PEM.
//! Client and server configurations load and validate it.

use std::path::PathBuf;
use std::sync::Arc;

use zeroize::Zeroizing;

use crate::{PemRole, TlsError};

/// A certificate-chain or trust-root PEM source.
///
/// | Constructor          | Behavior                                |
/// |----------------------|-----------------------------------------|
/// | [`PemSource::file`]  | Reads the file when settings are loaded |
/// | [`PemSource::bytes`] | Shares in-memory bytes between clones   |
///
/// File paths remain visible in `Debug`.
/// In-memory PEM is redacted.
///
/// ## See Also
///
/// - [`TlsIdentity`](crate::TlsIdentity) uses this type for a certificate chain.
/// - [`TrustRoots`](crate::TrustRoots) uses this type for trust roots.
#[derive(Clone)]
pub struct PemSource(PemSourceInner);

#[derive(Clone)]
enum PemSourceInner {
    Path(PathBuf),
    Bytes(Arc<Vec<u8>>),
}

impl PemSource {
    /// Creates a PEM source from a file path.
    ///
    /// This method only stores the path.
    ///
    /// ```
    /// use solti_tls::PemSource;
    ///
    /// let source = PemSource::file("/etc/solti/tls/server.crt");
    /// assert!(format!("{source:?}").contains("server.crt"));
    /// ```
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self(PemSourceInner::Path(path.into()))
    }

    /// Creates a PEM source from bytes already present in memory.
    ///
    /// Clones share the same immutable buffer.
    ///
    /// ```
    /// use solti_tls::PemSource;
    ///
    /// let source = PemSource::bytes(b"certificate PEM");
    /// assert!(format!("{source:?}").contains("redacted"));
    /// ```
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(PemSourceInner::Bytes(Arc::new(bytes.into())))
    }

    pub(crate) fn load(self, role: PemRole) -> Result<Vec<u8>, TlsError> {
        match self.0 {
            PemSourceInner::Path(path) => {
                std::fs::read(&path).map_err(|source| TlsError::ReadPem { role, path, source })
            }
            PemSourceInner::Bytes(bytes) => {
                Ok(Arc::try_unwrap(bytes).unwrap_or_else(|bytes| (*bytes).clone()))
            }
        }
    }
}

impl std::fmt::Debug for PemSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            PemSourceInner::Path(path) => f.debug_tuple("Path").field(path).finish(),
            PemSourceInner::Bytes(bytes) => {
                write!(f, "Bytes([{} bytes redacted])", bytes.len())
            }
        }
    }
}

/// A private-key PEM source.
///
/// | Constructor                 | Behavior                                      |
/// |-----------------------------|-----------------------------------------------|
/// | [`PrivateKeySource::file`]  | Reads the file when settings are loaded       |
/// | [`PrivateKeySource::bytes`] | Shares zeroizing bytes between clones         |
///
/// ## Rules
///
/// - File paths remain visible in `Debug`.
/// - In-memory key bytes are redacted.
/// - In-memory key bytes are zeroized after the last owner is dropped.
/// - Loaded identities keep key PEM in zeroizing storage.
/// - A TLS library or adapter may keep its own copy.
///
/// ## See Also
///
/// [`TlsIdentity`](crate::TlsIdentity) combines this source with a certificate chain.
#[derive(Clone)]
pub struct PrivateKeySource(PrivateKeySourceInner);

#[derive(Clone)]
enum PrivateKeySourceInner {
    Path(PathBuf),
    Bytes(Arc<Zeroizing<Vec<u8>>>),
}

impl PrivateKeySource {
    /// Creates a private-key source from a file path.
    ///
    /// This method only stores the path.
    /// Loading moves the file bytes into zeroizing storage.
    ///
    /// ```
    /// use solti_tls::PrivateKeySource;
    ///
    /// let source = PrivateKeySource::file("/etc/solti/tls/server.key");
    /// assert!(format!("{source:?}").contains("server.key"));
    /// ```
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self(PrivateKeySourceInner::Path(path.into()))
    }

    /// Creates a private-key source from bytes already present in memory.
    ///
    /// Clones share the same zeroizing buffer.
    ///
    /// ```
    /// use solti_tls::PrivateKeySource;
    ///
    /// let source = PrivateKeySource::bytes(b"private-key PEM");
    /// let debug = format!("{source:?}");
    /// assert!(debug.contains("redacted"));
    /// assert!(!debug.contains("private-key PEM"));
    /// ```
    pub fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(PrivateKeySourceInner::Bytes(Arc::new(Zeroizing::new(
            bytes.into(),
        ))))
    }

    pub(crate) fn load(self, role: PemRole) -> Result<Zeroizing<Vec<u8>>, TlsError> {
        match self.0 {
            PrivateKeySourceInner::Path(path) => std::fs::read(&path)
                .map(Zeroizing::new)
                .map_err(|source| TlsError::ReadPem { role, path, source }),
            PrivateKeySourceInner::Bytes(bytes) => Ok(Arc::try_unwrap(bytes)
                .unwrap_or_else(|bytes| Zeroizing::new(bytes.as_slice().to_vec()))),
        }
    }
}

impl std::fmt::Debug for PrivateKeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            PrivateKeySourceInner::Path(path) => f.debug_tuple("Path").field(path).finish(),
            PrivateKeySourceInner::Bytes(bytes) => {
                write!(f, "Bytes([{} private-key bytes redacted])", bytes.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn debug_redacts_in_memory_pem() {
        let source = PemSource::bytes(vec![1, 2, 3, 255]);
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("255"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn debug_redacts_private_key() {
        let source = PrivateKeySource::bytes(vec![201, 202, 203]);
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("201"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn load_reads_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"pem bytes").unwrap();
        let source = PemSource::file(file.path());
        assert_eq!(
            source.load(PemRole::ServerCertificate).unwrap(),
            b"pem bytes"
        );
    }

    #[test]
    fn read_error_keeps_role_and_path() {
        let path = PathBuf::from("/definitely/does/not/exist.pem");
        let error = PemSource::file(path.clone())
            .load(PemRole::ServerTrustRoots)
            .unwrap_err();
        assert!(matches!(
            error,
            TlsError::ReadPem {
                role: PemRole::ServerTrustRoots,
                path: error_path,
                ..
            } if error_path == path
        ));
    }
}
