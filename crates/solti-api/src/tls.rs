//! # TLS adapters for the API transports.
//!
//! Bridges [`solti_tls::ServerTlsConfig`] to the transport-specific TLS config that tonic / axum-server expect.
//! Available with the `tls` feature.
//!
//! ## Example
//!
//! ```rust,no_run
//! # use std::sync::Arc;
//! use solti_api::{GrpcApi, SupervisorApiAdapter, to_tonic_server_tls};
//! use solti_tls::ServerTlsConfig;
//!
//! # async fn serve(adapter: Arc<SupervisorApiAdapter>) -> Result<(), Box<dyn std::error::Error>> {
//! let server_tls = ServerTlsConfig::builder()
//!     .cert_pem_file("/etc/solti/tls/server.crt")
//!     .key_pem_file("/etc/solti/tls/server.key")
//!     .require_client_ca_pem_file("/etc/solti/tls/clients-ca.crt")
//!     .build()?;
//!
//! let tls_cfg = to_tonic_server_tls(&server_tls)?;
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
/// Reads PEM bytes via [`solti_tls::PemSource`] and feeds them to tonic's PEM-blob constructors.
/// mTLS is enabled when `client_ca` is set on the source config.
///
/// ## Errors
///
/// - [`TlsError::Io`]: a [`PemSource::Path`] (cert, key, or client CA) could not be read.
///
/// ## Notes on mTLS
///
/// When `client_ca` is set, this helper sets `client_ca_root` on the tonic config,
/// leaving `client_auth_optional` at its default (`false`) - i.e. **client cert is required**, matching `solti-tls`'s server semantics.
///
/// [`PemSource::Path`]: solti_tls::PemSource::Path
pub fn to_tonic_server_tls(cfg: &ServerTlsConfig) -> Result<TonicServerTls, TlsError> {
    let cert_bytes = cfg.cert.read()?;
    let key_bytes = cfg.key.read()?;

    let mut tls = TonicServerTls::new().identity(Identity::from_pem(cert_bytes, key_bytes));

    if let Some(ca_src) = &cfg.client_ca {
        let ca_bytes = ca_src.read()?;
        tls = tls.client_ca_root(Certificate::from_pem(ca_bytes));
    }

    Ok(tls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_tls::ServerTlsConfig;

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
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(cert)
            .key_pem_bytes(key)
            .build()
            .unwrap();
        let _tls = to_tonic_server_tls(&cfg).unwrap();
    }

    #[test]
    fn to_tonic_server_tls_includes_client_ca_for_mtls() {
        let (cert, key) = rcgen_self_signed();
        let (ca, _) = rcgen_self_signed();
        let cfg = ServerTlsConfig::builder()
            .cert_pem_bytes(cert)
            .key_pem_bytes(key)
            .require_client_ca_pem_bytes(ca)
            .build()
            .unwrap();
        let _tls = to_tonic_server_tls(&cfg).unwrap();
    }

    #[test]
    fn to_tonic_server_tls_propagates_io_error_for_missing_cert_path() {
        let cfg = ServerTlsConfig::builder()
            .cert_pem_file("/nonexistent/server.crt")
            .key_pem_file("/nonexistent/server.key")
            .build()
            .unwrap();
        let err = to_tonic_server_tls(&cfg).unwrap_err();
        assert!(matches!(err, TlsError::Io(_)));
    }
}
