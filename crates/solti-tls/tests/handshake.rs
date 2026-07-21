//! End-to-end TLS handshake tests using `solti-tls` on both sides.

use std::net::IpAddr;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, SanType};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
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

#[tokio::test]
async fn plain_tls_round_trip_via_solti_configs() {
    let pki = make_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem,
        pki.server_key_pem,
    ))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;

    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();
    let mut tls = connector.connect(server_name, stream).await.unwrap();

    tls.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    tls.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
}

#[tokio::test]
async fn mtls_round_trip_via_solti_configs() {
    let pki = make_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem,
        pki.server_key_pem,
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem))
        .with_identity(TlsIdentity::from_pem_bytes(
            pki.client_cert_pem,
            pki.client_key_pem,
        ))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;

    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();
    let mut tls = connector.connect(server_name, stream).await.unwrap();

    tls.write_all(b"mTLS!").await.unwrap();
    let mut buf = [0u8; 5];
    tls.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"mTLS!");
}

#[tokio::test]
async fn client_rejects_server_cert_from_untrusted_ca() {
    let server_pki = make_pki();
    let other_pki = make_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        server_pki.server_cert_pem,
        server_pki.server_key_pem,
    ))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(other_pki.ca_cert_pem))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();

    let result = connector.connect(server_name, stream).await;
    assert!(
        result.is_err(),
        "client must reject a server cert that does not chain to its trusted CA"
    );
}

#[tokio::test]
async fn mtls_server_rejects_client_cert_from_untrusted_ca() {
    let server_pki = make_pki();
    let other_pki = make_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        server_pki.server_cert_pem,
        server_pki.server_key_pem,
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(server_pki.ca_cert_pem.clone()))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(server_pki.ca_cert_pem))
        .with_identity(TlsIdentity::from_pem_bytes(
            other_pki.client_cert_pem,
            other_pki.client_key_pem,
        ))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();

    let connect_result = connector.connect(server_name, stream).await;
    let mut tls = match connect_result {
        Ok(tls) => tls,
        Err(_) => return,
    };
    let write_res = tls.write_all(b"hello").await;
    let mut buf = [0u8; 5];
    let read_res = tls.read_exact(&mut buf).await;
    assert!(
        write_res.is_err() || read_res.is_err(),
        "mTLS server must reject a client cert from an untrusted CA (write={write_res:?}, read={read_res:?})"
    );
}

#[tokio::test]
async fn mtls_server_rejects_client_without_cert() {
    let pki = make_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem,
        pki.server_key_pem,
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_cert_pem.clone()))
    .into_rustls_config()
    .unwrap();

    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;

    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();

    let connect_result = connector.connect(server_name, stream).await;
    let mut tls = match connect_result {
        Ok(tls) => tls,
        Err(_) => return,
    };

    let write_res = tls.write_all(b"hello").await;
    let mut buf = [0u8; 5];
    let read_res = tls.read_exact(&mut buf).await;

    assert!(
        write_res.is_err() || read_res.is_err(),
        "mTLS server must reject client that doesn't present a cert (write_res = {write_res:?}, read_res = {read_res:?})"
    );
}

#[tokio::test]
async fn client_rejects_certificate_for_wrong_server_name() {
    let pki = make_pki();

    let server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem,
        pki.server_key_pem,
    ))
    .into_rustls_config()
    .unwrap();
    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let wrong_name = ServerName::try_from("localhost").unwrap();

    assert!(
        connector.connect(wrong_name, stream).await.is_err(),
        "client must reject a trusted certificate whose SAN does not match the server name"
    );
}

#[tokio::test]
async fn transport_owned_alpn_is_negotiated() {
    let pki = make_pki();

    let mut server_cfg = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_cert_pem,
        pki.server_key_pem,
    ))
    .into_rustls_config()
    .unwrap();
    server_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let mut client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_cert_pem))
        .into_rustls_config()
        .unwrap();
    client_cfg.alpn_protocols = vec![b"h2".to_vec()];

    let port = run_echo_server(server_cfg).await;
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();
    let tls = connector.connect(server_name, stream).await.unwrap();

    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
}

#[tokio::test]
async fn file_sources_complete_a_tls_handshake() {
    let pki = make_pki();
    let directory = tempfile::tempdir().unwrap();
    let server_certificate = directory.path().join("server.crt");
    let server_key = directory.path().join("server.key");
    let server_ca = directory.path().join("server-ca.crt");
    std::fs::write(&server_certificate, pki.server_cert_pem).unwrap();
    std::fs::write(&server_key, pki.server_key_pem).unwrap();
    std::fs::write(&server_ca, pki.ca_cert_pem).unwrap();

    let server_cfg =
        ServerTlsConfig::new(TlsIdentity::from_pem_files(server_certificate, server_key))
            .into_rustls_config()
            .unwrap();
    let client_cfg = ClientTlsConfig::new(TrustRoots::from_pem_file(server_ca))
        .into_rustls_config()
        .unwrap();

    let port = run_echo_server(server_cfg).await;
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let server_name = ServerName::try_from("127.0.0.1").unwrap();
    let mut tls = connector.connect(server_name, stream).await.unwrap();
    tls.write_all(b"files").await.unwrap();
    let mut response = [0u8; 5];
    tls.read_exact(&mut response).await.unwrap();
    assert_eq!(&response, b"files");
}
