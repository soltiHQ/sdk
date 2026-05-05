//! PEM cert/key parsing helpers.

use std::io::BufRead;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::TlsError;

/// Parse all `CERTIFICATE` sections out of a PEM-encoded byte stream.
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

/// Parse the first private key (`PKCS#8`, `PKCS#1`, or `SEC1`) out of a PEM-encoded byte stream.
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
