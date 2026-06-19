//! # PEM source: file path or in-memory buffer.

use std::path::PathBuf;

use crate::TlsError;

/// Where a PEM blob lives.
#[derive(Clone)]
pub enum PemSource {
    /// PEM file on disk; read at `into_rustls_config()` time.
    Path(PathBuf),
    /// Already-loaded PEM bytes (maybe cert, CA, or **private key** material).
    Bytes(Vec<u8>),
}

impl std::fmt::Debug for PemSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PemSource::Path(p) => f.debug_tuple("Path").field(p).finish(),
            PemSource::Bytes(b) => write!(f, "Bytes([{} bytes redacted])", b.len()),
        }
    }
}

impl PemSource {
    /// Read the PEM bytes from the source.
    ///
    /// - [`PemSource::Path`] performs file I/O (errors surface as [`TlsError::Io`]).
    /// - [`PemSource::Bytes`] returns a fresh **clone** of the buffer.
    ///
    /// ## Also
    ///
    /// - [`load_certs_from_pem`](crate::load_certs_from_pem) / [`load_key_from_pem`](crate::load_key_from_pem) - the usual consumers of the returned bytes.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_tls::PemSource;
    ///
    /// let src = PemSource::Bytes(b"-----BEGIN CERTIFICATE-----\n...".to_vec());
    /// let bytes = src.read().unwrap();
    /// assert!(bytes.starts_with(b"-----BEGIN"));
    /// ```
    pub fn read(&self) -> Result<Vec<u8>, TlsError> {
        match self {
            PemSource::Path(p) => Ok(std::fs::read(p)?),
            PemSource::Bytes(b) => Ok(b.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn debug_does_not_leak_secret_bytes() {
        let src = PemSource::Bytes(vec![1, 2, 3, 255]);
        let rendered = format!("{src:?}");
        assert!(
            !rendered.contains("255"),
            "raw key bytes must not appear in Debug output: {rendered}"
        );
        assert!(
            rendered.to_lowercase().contains("redact"),
            "Debug should show a redaction marker, got: {rendered}"
        );
    }

    #[test]
    fn debug_path_variant_is_shown() {
        let src = PemSource::Path("/etc/tls/server.crt".into());
        let rendered = format!("{src:?}");
        assert!(
            rendered.contains("server.crt"),
            "path should be visible: {rendered}"
        );
    }

    #[test]
    fn read_returns_bytes_variant_verbatim() {
        let src = PemSource::Bytes(b"hello pem".to_vec());
        let out = src.read().unwrap();
        assert_eq!(out, b"hello pem");
    }

    #[test]
    fn read_loads_path_variant_from_disk() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"on-disk bytes").unwrap();

        let src = PemSource::Path(tmp.path().to_path_buf());
        let out = src.read().unwrap();
        assert_eq!(out, b"on-disk bytes");
    }

    #[test]
    fn read_returns_io_error_for_missing_path() {
        let src = PemSource::Path("/definitely/does/not/exist.pem".into());
        let err = src.read().unwrap_err();
        assert!(matches!(err, crate::TlsError::Io(_)));
    }
}
