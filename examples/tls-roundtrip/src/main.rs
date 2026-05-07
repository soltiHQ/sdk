//! # tls-roundtrip
//!
//! Self-contained mTLS demo of `solti-tls` in action.
//!
//! Generates an in-memory PKI (CA + server cert with `SAN=127.0.0.1` + client
//! cert) via `rcgen`, then runs both transports side by side:
//!
//! - **HTTPS**: `axum` router behind `axum-server` with the `rustls::ServerConfig`
//!   produced by [`solti_tls::ServerTlsConfig::into_rustls_config`].
//! - **gRPC**: `tonic::transport::Server` with the
//!   [`tonic::transport::ServerTlsConfig`] returned by
//!   [`solti_api::to_tonic_server_tls`], serving the standard
//!   `grpc.health.v1.Health` service from `tonic-health`.
//!
//! Both servers require a client certificate signed by the same CA. A
//! `reqwest` client and a `tonic` client connect with their cert presented,
//! exchange one round trip each, and the program exits when both succeed.
//!
//! ```bash
//! cargo run -p tls-roundtrip
//! ```
//!
//! Expected output:
//! ```text
//! HTTPS  listening on 127.0.0.1:18443
//! gRPC   listening on 127.0.0.1:18444
//! HTTPS  client received: "hello, mTLS!"
//! gRPC   client received: SERVING
//! mTLS round-trip OK
//! ```
//!
//! No supervisor, no discovery, no metrics — just `solti-tls` end-to-end.
//! For the production cert-delivery patterns (certbot, cert-manager,
//! rustls-acme) see `crates/solti-tls/README.md`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::{Router, routing::get};
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, SanType};
use tokio::time::sleep;
use tonic::transport::{
    Certificate as TonicCert, ClientTlsConfig as TonicClientTls, Endpoint, Identity, Server,
};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tracing::info;

use solti_api::to_tonic_server_tls;
use solti_tls::{ClientTlsConfig, ServerTlsConfig};

const HTTP_ADDR: &str = "127.0.0.1:18443";
const GRPC_ADDR: &str = "127.0.0.1:18444";

/// In-memory PKI: a self-signed CA + server cert (SAN = 127.0.0.1) + client cert.
struct Pki {
    ca_pem: Vec<u8>,
    server_cert_pem: Vec<u8>,
    server_key_pem: Vec<u8>,
    client_cert_pem: Vec<u8>,
    client_key_pem: Vec<u8>,
}

fn make_pki() -> anyhow::Result<Pki> {
    // Self-signed CA.
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "tls-roundtrip-ca");
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params.self_signed(&ca_key)?;

    // Borrow CA params + key into a reusable Issuer (rcgen 0.14 API).
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    // Server cert with SAN = 127.0.0.1 so rustls accepts the IP-literal client URI.
    let mut srv_params = CertificateParams::default();
    srv_params.subject_alt_names = vec![SanType::IpAddress(IpAddr::from([127, 0, 0, 1]))];
    srv_params
        .distinguished_name
        .push(DnType::CommonName, "tls-roundtrip-server");
    let srv_key = KeyPair::generate()?;
    let srv_cert = srv_params.signed_by(&srv_key, &issuer)?;

    // Client cert (used for mTLS).
    let mut cli_params = CertificateParams::default();
    cli_params
        .distinguished_name
        .push(DnType::CommonName, "tls-roundtrip-client");
    let cli_key = KeyPair::generate()?;
    let cli_cert = cli_params.signed_by(&cli_key, &issuer)?;

    Ok(Pki {
        ca_pem: ca_cert.pem().into_bytes(),
        server_cert_pem: srv_cert.pem().into_bytes(),
        server_key_pem: srv_key.serialize_pem().into_bytes(),
        client_cert_pem: cli_cert.pem().into_bytes(),
        client_key_pem: cli_key.serialize_pem().into_bytes(),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .compact()
        .init();

    let pki = make_pki().context("generating in-memory PKI")?;

    // ---- Server: HTTPS half (axum + axum-server + mTLS) ----
    let http_server_cfg = ServerTlsConfig::builder()
        .cert_pem_bytes(pki.server_cert_pem.clone())
        .key_pem_bytes(pki.server_key_pem.clone())
        .require_client_ca_pem_bytes(pki.ca_pem.clone())
        .with_alpn(["h2", "http/1.1"])
        .build()?
        .into_rustls_config()?;

    let http_addr: SocketAddr = HTTP_ADDR.parse()?;
    let app = Router::new().route("/hello", get(|| async { "hello, mTLS!" }));
    let http_handle = tokio::spawn(async move {
        let result = axum_server::bind_rustls(
            http_addr,
            RustlsConfig::from_config(Arc::new(http_server_cfg)),
        )
        .serve(app.into_make_service())
        .await;
        if let Err(e) = result {
            tracing::error!(?e, "HTTPS server exited with error");
        }
    });
    info!("HTTPS  listening on {http_addr}");

    // ---- Server: gRPC half (tonic + solti_api::to_tonic_server_tls + mTLS) ----
    let grpc_server_cfg = ServerTlsConfig::builder()
        .cert_pem_bytes(pki.server_cert_pem.clone())
        .key_pem_bytes(pki.server_key_pem.clone())
        .require_client_ca_pem_bytes(pki.ca_pem.clone())
        .build()?;
    let grpc_tonic_tls = to_tonic_server_tls(&grpc_server_cfg)?;

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    // Mark the empty-string service ("") — the default — as SERVING.
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let grpc_addr: SocketAddr = GRPC_ADDR.parse()?;
    let grpc_builder = Server::builder()
        .tls_config(grpc_tonic_tls)?
        .add_service(health_service);
    let grpc_handle = tokio::spawn(async move {
        if let Err(e) = grpc_builder.serve(grpc_addr).await {
            tracing::error!(?e, "gRPC server exited with error");
        }
    });
    info!("gRPC   listening on {grpc_addr}");

    // Give both servers a moment to bind their listeners.
    sleep(Duration::from_millis(250)).await;

    // ---- Client: HTTPS round trip (reqwest + use_preconfigured_tls) ----
    let client_cfg = ClientTlsConfig::builder()
        .ca_pem_bytes(pki.ca_pem.clone())
        .client_cert_pem_bytes(pki.client_cert_pem.clone())
        .client_key_pem_bytes(pki.client_key_pem.clone())
        .build()?
        .into_rustls_config()?;
    let http_client = reqwest::Client::builder()
        .use_preconfigured_tls(client_cfg)
        .build()?;

    let body = http_client
        .get(format!("https://{HTTP_ADDR}/hello"))
        .send()
        .await
        .context("HTTPS request failed")?
        .error_for_status()?
        .text()
        .await?;
    info!("HTTPS  client received: {body:?}");
    anyhow::ensure!(body == "hello, mTLS!", "unexpected HTTPS body: {body:?}");

    // ---- Client: gRPC round trip (tonic Channel with mTLS) ----
    // tonic does not accept a pre-built rustls::ClientConfig, so we hand it
    // PEM bytes directly through tonic::transport::ClientTlsConfig — same
    // pattern as solti_discover's internal converter.
    let tonic_client_tls = TonicClientTls::new()
        .ca_certificate(TonicCert::from_pem(pki.ca_pem.clone()))
        .identity(Identity::from_pem(
            pki.client_cert_pem.clone(),
            pki.client_key_pem.clone(),
        ))
        .domain_name("127.0.0.1");

    let channel = Endpoint::from_shared(format!("https://{GRPC_ADDR}"))?
        .tls_config(tonic_client_tls)?
        .connect()
        .await
        .context("gRPC connect failed")?;

    let mut health = HealthClient::new(channel);
    let resp = health
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .context("gRPC Health.Check failed")?;
    let status =
        ServingStatus::try_from(resp.into_inner().status).unwrap_or(ServingStatus::Unknown);
    let label = match status {
        ServingStatus::Serving => "SERVING",
        ServingStatus::NotServing => "NOT_SERVING",
        ServingStatus::ServiceUnknown => "SERVICE_UNKNOWN",
        ServingStatus::Unknown => "UNKNOWN",
    };
    info!("gRPC   client received: {label}");
    anyhow::ensure!(matches!(status, ServingStatus::Serving), "expected SERVING");

    info!("mTLS round-trip OK");

    // Tear down the servers — neither completes naturally; abort their tasks.
    http_handle.abort();
    grpc_handle.abort();
    Ok(())
}
