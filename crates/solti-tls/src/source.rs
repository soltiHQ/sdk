//! Abstract source of PEM bytes: file path or in-memory buffer.

use std::path::PathBuf;

use crate::TlsError;

/// Where a PEM blob lives.
#[derive(Debug, Clone)]
pub enum PemSource {
    /// PEM file on disk; read at `into_rustls_config()` time.
    Path(PathBuf),
    /// Already-loaded PEM bytes.
    Bytes(Vec<u8>),
}

impl PemSource {
    /// Read the PEM bytes from source:
    /// - [`PemSource::Path`] opens the file;
    /// - [`PemSource::Bytes`] it returns a clone of the in-memory buffer.
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
