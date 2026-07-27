//! # TLS adapters for the API transports.
//!
//! Bridges [`solti_tls::ServerTlsConfig`] to tonic's server TLS configuration.
//! Available with the `grpc-tls` feature.
//!
//! ## Example
//!
//! ```rust,no_run
//! # use std::sync::Arc;
//! use solti_api::{GrpcApi, SupervisorApiAdapter, to_tonic_server_tls};
//! use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};
//!
//! # async fn serve(adapter: Arc<SupervisorApiAdapter>) -> Result<(), Box<dyn std::error::Error>> {
//! let server_tls = ServerTlsConfig::new(TlsIdentity::from_pem_files(
//!     "/etc/solti/tls/server.crt",
//!     "/etc/solti/tls/server.key",
//! ))
//! .require_client_auth(TrustRoots::from_pem_file(
//!     "/etc/solti/tls/clients-ca.crt",
//! ));
//!
//! let tls_cfg = to_tonic_server_tls(server_tls)?;
//! tonic::transport::Server::builder()
//!     .tls_config(tls_cfg)?
//!     .add_service(GrpcApi::new(adapter).server())
//!     .serve("0.0.0.0:50443".parse()?)
//!     .await?;
//! # Ok(()) }
//! ```

use solti_tls::{ServerTlsConfig, TlsError};
use tonic::transport::{Certificate, Identity, ServerTlsConfig as TonicServerTls};

/// Convert [`solti_tls::ServerTlsConfig`] into [`tonic::transport::ServerTlsConfig`].
///
/// Loads the structured PEM material and feeds it to tonic's PEM constructors.
/// mTLS is enabled when client-auth roots are present.
///
/// ## Errors
///
/// - [`TlsError::ReadPem`]: a configured PEM file could not be read.
///
/// ## Notes on mTLS
///
/// When client-auth roots are set, this helper sets `client_ca_root` on the tonic config,
/// leaving `client_auth_optional` at its default (`false`) - i.e. **client cert is required**, matching `solti-tls`'s server semantics.
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
