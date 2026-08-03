//! End-to-end TLS handshake tests using `solti-tls` on both sides.

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, SanType};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use solti_tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};

struct Pki {
    ca_cert_pem: Vec<u8>,
    server_cert_pem: Vec<u8>,
    server_key_pem: Vec<u8>,
    client_cert_pem: Vec<u8>,
    client_key_pem: Vec<u8>,
}

fn make_pki() -> Pki {
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "solti-test-ca");
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();

    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut server_params = CertificateParams::default();
    server_params.subject_alt_names = vec![SanType::IpAddress(IpAddr::from([127, 0, 0, 1]))];
    server_params
        .distinguished_name
        .push(DnType::CommonName, "solti-test-server");
    let server_key = KeyPair::generate().unwrap();
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

    let mut client_params = CertificateParams::default();
    client_params
        .distinguished_name
        .push(DnType::CommonName, "solti-test-client");
    let client_key = KeyPair::generate().unwrap();
    let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();

    Pki {
        ca_cert_pem: ca_cert.pem().into_bytes(),
        server_cert_pem: server_cert.pem().into_bytes(),
        server_key_pem: server_key.serialize_pem().into_bytes(),
        client_cert_pem: client_cert.pem().into_bytes(),
        client_key_pem: client_key.serialize_pem().into_bytes(),
    }
}

fn primary_pki() -> &'static Pki {
    static PKI: OnceLock<Pki> = OnceLock::new();
    PKI.get_or_init(make_pki)
}

fn other_pki() -> &'static Pki {
    static PKI: OnceLock<Pki> = OnceLock::new();
    PKI.get_or_init(make_pki)
}

async fn run_echo_server(server_cfg: rustls::ServerConfig) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));
        if let Ok((stream, _)) = listener.accept().await
            && let Ok(mut tls) = acceptor.accept(stream).await
        {
            let mut buf = [0u8; 5];
            if tls.read_exact(&mut buf).await.is_ok() {
                let _ = tls.write_all(&buf).await;
                let _ = tls.shutdown().await;
            }
        }
    });

    port
}

async fn connect(
    client_cfg: rustls::ClientConfig,
    port: u16,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    connect_as(client_cfg, port, "127.0.0.1").await
}

async fn connect_as(
    client_cfg: rustls::ClientConfig,
    port: u16,
    server_name: &'static str,
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let server_name = ServerName::try_from(server_name).unwrap();
    connector.connect(server_name, stream).await
}

async fn assert_round_trip(client_cfg: rustls::ClientConfig, port: u16, payload: &[u8; 5]) {
    let mut tls = connect(client_cfg, port).await.unwrap();
    tls.write_all(payload).await.unwrap();

    let mut response = [0; 5];
    tls.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, payload);
}

async fn assert_server_rejects(client_cfg: rustls::ClientConfig, port: u16) {
    let Ok(mut tls) = connect(client_cfg, port).await else {
        return;
    };

    let write = tls.write_all(b"hello").await;
    let mut response = [0; 5];
    let read = tls.read_exact(&mut response).await;
    assert!(
        write.is_err() || read.is_err(),
        "mTLS server accepted an unauthorized client"
    );
}

#[tokio::test]
async fn plain_tls_round_trip_via_solti_configs() {
    let pki = primary_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem.clone(),
        pki.server_key_pem.clone(),
    ))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    assert_round_trip(client_cfg, port, b"hello").await;
}

#[tokio::test]
async fn mtls_round_trip_via_solti_configs() {
    let pki = primary_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem.clone(),
        pki.server_key_pem.clone(),
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
        .with_identity(TlsIdentity::from_pem_bytes(
            pki.client_cert_pem.clone(),
            pki.client_key_pem.clone(),
        ))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    assert_round_trip(client_cfg, port, b"mTLS!").await;
}

#[tokio::test]
async fn client_rejects_server_cert_from_untrusted_ca() {
    let server_pki = primary_pki();
    let other_pki = other_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        server_pki.server_cert_pem.clone(),
        server_pki.server_key_pem.clone(),
    ))
    .into_rustls_config()
    .unwrap();

    let client_cfg =
        ClientTlsConfig::new(TrustRoots::from_pem_bytes(other_pki.ca_cert_pem.clone()))
            .into_rustls_config()
            .unwrap();

    let port = run_echo_server(server_cfg).await;
    assert!(
        connect(client_cfg, port).await.is_err(),
        "client must reject a server cert that does not chain to its trusted CA"
    );
}

#[tokio::test]
async fn client_rejects_certificate_for_wrong_server_name() {
    let pki = primary_pki();
    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem.clone(),
        pki.server_key_pem.clone(),
    ))
    .into_rustls_config()
    .unwrap();
    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    assert!(
        connect_as(client_cfg, port, "localhost").await.is_err(),
        "client must reject a certificate for another server name"
    );
}

#[tokio::test]
async fn mtls_server_rejects_client_cert_from_untrusted_ca() {
    let server_pki = primary_pki();
    let other_pki = other_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        server_pki.server_cert_pem.clone(),
        server_pki.server_key_pem.clone(),
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(server_pki.ca_cert_pem.clone()))
    .into_rustls_config()
    .unwrap();

    let client_cfg =
        ClientTlsConfig::new(TrustRoots::from_pem_bytes(server_pki.ca_cert_pem.clone()))
            .with_identity(TlsIdentity::from_pem_bytes(
                other_pki.client_cert_pem.clone(),
                other_pki.client_key_pem.clone(),
            ))
            .into_rustls_config()
            .unwrap();

    let port = run_echo_server(server_cfg).await;
    assert_server_rejects(client_cfg, port).await;
}

#[tokio::test]
async fn mtls_server_rejects_client_without_cert() {
    let pki = primary_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem.clone(),
        pki.server_key_pem.clone(),
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    assert_server_rejects(client_cfg, port).await;
}
