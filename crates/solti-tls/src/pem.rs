//! # PEM to DER parsing helpers.

use std::io::BufRead;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::TlsError;

/// Parse all `CERTIFICATE` blocks from a PEM byte stream.
///
/// Certificates are returned in file order.
/// For a certificate chain, put the leaf certificate first.
///
/// ## Also
///
/// - [`PemSource::read`](crate::PemSource::read) - produces the bytes fed here.
/// - [`load_key_from_pem`] - the private-key counterpart.
///
/// ## Example
///
/// ```
/// use solti_tls::{load_certs_from_pem, TlsError};
///
/// let err = load_certs_from_pem(&b"not a pem"[..]).unwrap_err();
/// assert!(matches!(err, TlsError::NoCertificates));
/// ```
pub fn load_certs_from_pem<R: BufRead>(
    reader: R,
) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut reader = reader;

    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut reader).collect();
    let certs = certs?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificates);
    }
    Ok(certs)
}

/// Parse the first private key from a PEM byte stream.
///
/// Supported key forms are `PKCS#8`, `PKCS#1`, and `SEC1`.
/// If the stream contains more than one key, only the first one is returned.
/// Pass a PEM that holds exactly the intended key.
///
/// ## Also
///
/// - [`load_certs_from_pem`] - the certificate counterpart (which, unlike this function, returns *all* blocks).
///
/// ## Example
///
/// ```
/// use solti_tls::{load_key_from_pem, TlsError};
///
/// let err = load_key_from_pem(&b"not a key"[..]).unwrap_err();
/// assert!(matches!(err, TlsError::NoPrivateKey));
/// ```
pub fn load_key_from_pem<R: BufRead>(reader: R) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut reader = reader;
    let key = rustls_pemfile::private_key(&mut reader)?;
    key.ok_or(TlsError::NoPrivateKey)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed_cert_pem() -> String {
        let bundle = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        bundle.cert.pem()
    }

    #[test]
    fn load_certs_from_pem_returns_one_cert_for_one_pem() {
        let pem = self_signed_cert_pem();
        let certs = load_certs_from_pem(pem.as_bytes()).unwrap();
        assert_eq!(certs.len(), 1);
    }

    #[test]
    fn load_certs_from_pem_errors_when_input_has_no_certs() {
        let err = load_certs_from_pem(&b""[..]).unwrap_err();
        assert!(matches!(err, TlsError::NoCertificates));
    }

    #[test]
    fn load_certs_from_pem_errors_when_input_has_only_a_private_key() {
        let bundle = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        let key_pem = bundle.signing_key.serialize_pem();
        let err = load_certs_from_pem(key_pem.as_bytes()).unwrap_err();
        assert!(matches!(err, TlsError::NoCertificates));
    }

    #[test]
    fn load_key_from_pem_returns_pkcs8_for_rcgen_default() {
        let bundle = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        let key_pem = bundle.signing_key.serialize_pem();
        let key = load_key_from_pem(key_pem.as_bytes()).unwrap();
        assert!(matches!(key, PrivateKeyDer::Pkcs8(_)));
    }

    #[test]
    fn load_key_from_pem_errors_when_input_has_no_key() {
        let pem = self_signed_cert_pem();
        let err = load_key_from_pem(pem.as_bytes()).unwrap_err();
        assert!(matches!(err, TlsError::NoPrivateKey));
    }
}
