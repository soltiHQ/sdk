//! # gRPC TLS
//!
//! Adapter from [`solti_tls::ServerTlsConfig`] to tonic.
//! This module is available with feature `grpc-tls`.
//!
//! ```text
//! certificate + private key + optional client roots
//!                         │
//!                         ▼
//!            solti_tls::ServerTlsConfig
//!                         │ validate and load
//!                         ▼
//!         tonic::transport::ServerTlsConfig
//! ```
//!
//! Client roots enable mandatory client certificate authentication.
//!
//! ## Example
//!
//! ```rust,no_run
//! # use std::sync::Arc;
//! use solti_api::{ApiHandler, GrpcApi, to_tonic_server_tls};
//! use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};
//!
//! # async fn serve<H: ApiHandler>(handler: Arc<H>) -> Result<(), Box<dyn std::error::Error>> {
//! let server_tls = ServerTlsConfig::new(TlsIdentity::from_pem_files(
//!     "/etc/solti/tls/server.crt",
//!     "/etc/solti/tls/server.key",
//! ))
//! .require_client_auth(TrustRoots::from_pem_file(
//!     "/etc/solti/tls/clients-ca.crt",
//! ));
//!
//! let tls_cfg = to_tonic_server_tls(server_tls)?;
//! solti_api::tonic::transport::Server::builder()
//!     .tls_config(tls_cfg)?
//!     .add_service(GrpcApi::new(handler).server())
//!     .serve("0.0.0.0:50443".parse()?)
//!     .await?;
//! # Ok(()) }
//! ```

use solti_tls::{ServerTlsConfig, TlsError};
use tonic::transport::{Certificate, Identity, ServerTlsConfig as TonicServerTls};

/// Converts Solti server TLS settings into tonic settings.
///
/// The input is fully loaded and validated first.
/// Client trust roots make client certificates mandatory.
///
/// ## Errors
///
/// Returns the loading and validation errors from
/// [`ServerTlsConfig::load`](solti_tls::ServerTlsConfig::load).
///
/// These include unreadable or invalid PEM, missing certificate or key blocks,
/// invalid trust roots, and invalid certificate-key configuration.
///
/// ## Security
///
/// The returned tonic config owns a copy of the private-key PEM.
pub fn to_tonic_server_tls(cfg: ServerTlsConfig) -> Result<TonicServerTls, TlsError> {
    let loaded = cfg.load()?;
    let identity = loaded.identity();

    let mut tls = TonicServerTls::new().identity(Identity::from_pem(
        identity.certificate_chain_pem(),
        identity.expose_private_key_pem(),
    ));

    if let Some(roots) = loaded.client_auth_roots_pem() {
        tls = tls.client_ca_root(Certificate::from_pem(roots));
    }

    Ok(tls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_tls::{PemRole, ServerTlsConfig, TlsIdentity, TrustRoots};

    fn rcgen_self_signed() -> (Vec<u8>, Vec<u8>) {
        let b = rcgen::generate_simple_self_signed(vec!["example.com".into()]).unwrap();
        (
            b.cert.pem().into_bytes(),
            b.signing_key.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn to_tonic_server_tls_succeeds_with_cert_and_key() {
        let (cert, key) = rcgen_self_signed();
        let cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(cert, key));
        let _tls = to_tonic_server_tls(cfg).unwrap();
    }

    #[test]
    fn to_tonic_server_tls_includes_client_ca_for_mtls() {
        let (cert, key) = rcgen_self_signed();
        let (ca, _) = rcgen_self_signed();
        let cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(cert, key))
            .require_client_auth(TrustRoots::from_pem_bytes(ca));
        let _tls = to_tonic_server_tls(cfg).unwrap();
    }

    #[test]
    fn to_tonic_server_tls_propagates_io_error_for_missing_cert_path() {
        let cfg = ServerTlsConfig::new(TlsIdentity::from_pem_files(
            "/nonexistent/server.crt",
            "/nonexistent/server.key",
        ));
        let err = to_tonic_server_tls(cfg).unwrap_err();
        assert!(matches!(
            err,
            TlsError::ReadPem {
                role: PemRole::ServerCertificate,
                ..
            }
        ));
    }
}
