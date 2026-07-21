//! # PEM parsing

use rustls::pki_types::{
    CertificateDer, PrivateKeyDer,
    pem::{PemObject, ReadIter},
};

use crate::{PemRole, TlsError};

pub(crate) fn load_certificates(
    pem: &[u8],
    role: PemRole,
) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certificates = CertificateDer::pem_reader_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| TlsError::InvalidPem { role, source })?;
    if certificates.is_empty() {
        return Err(TlsError::NoCertificates { role });
    }
    Ok(certificates)
}

pub(crate) fn load_private_key(
    pem: &[u8],
    role: PemRole,
) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut keys = ReadIter::<_, PrivateKeyDer<'static>>::new(pem);
    let key = match keys.next() {
        Some(Ok(key)) => key,
        Some(Err(source)) => return Err(TlsError::InvalidPem { role, source }),
        None => return Err(TlsError::NoPrivateKey { role }),
    };

    match keys.next() {
        Some(Ok(_)) => Err(TlsError::MultiplePrivateKeys { role }),
        Some(Err(source)) => Err(TlsError::InvalidPem { role, source }),
        None => Ok(key),
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
    fn parses_certificate_chain() {
        let (certificate, _) = self_signed();
        let certificates = load_certificates(&certificate, PemRole::ServerCertificate).unwrap();
        assert_eq!(certificates.len(), 1);
    }

    #[test]
    fn missing_certificate_keeps_role() {
        let error = load_certificates(b"not a certificate", PemRole::ClientTrustRoots).unwrap_err();
        assert!(matches!(
            error,
            TlsError::NoCertificates {
                role: PemRole::ClientTrustRoots
            }
        ));
    }

    #[test]
    fn malformed_certificate_is_not_reported_as_io() {
        let error = load_certificates(
            b"-----BEGIN CERTIFICATE-----\n%%%\n-----END CERTIFICATE-----",
            PemRole::ServerCertificate,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TlsError::InvalidPem {
                role: PemRole::ServerCertificate,
                ..
            }
        ));
    }

    #[test]
    fn parses_one_private_key() {
        let (_, key) = self_signed();
        let _key = load_private_key(&key, PemRole::ServerPrivateKey).unwrap();
    }

    #[test]
    fn rejects_multiple_private_keys() {
        let (_, first) = self_signed();
        let (_, second) = self_signed();
        let mut pem = first;
        pem.extend(second);
        let error = load_private_key(&pem, PemRole::ClientPrivateKey).unwrap_err();
        assert!(matches!(
            error,
            TlsError::MultiplePrivateKeys {
                role: PemRole::ClientPrivateKey
            }
        ));
    }
}
