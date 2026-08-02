//! # Mutual TLS round trip
//!
//! Both peers authenticate during an mTLS handshake.
//! The server presents a certificate trusted by the client.
//! The client presents a certificate trusted by the server.
//!
//! This example shows:
//!
//! - `require_client_auth` on the server;
//! - `with_identity` on the client;
//! - independent trust roots and identities;
//! - the server observing the presented client chain;
//! - an encrypted request and response.
//!
//! The PKI is generated only to keep the example self-contained.
//! Production applications normally load certificates issued outside the process.
//!
//! Run with `cargo run -p solti-tls --example mtls_round_trip`.

use std::{io, net::SocketAddr, sync::Arc};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::ServerName;
use solti_tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const MESSAGE: &[u8] = b"hello mTLS";

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-tls: mutual TLS

  shared example CA
      ├── signs client certificate ──► client TlsIdentity
      └── signs server certificate ──► server TlsIdentity

  client                                              server
  TlsIdentity(cert + key)                      TlsIdentity(cert + key)
  TrustRoots(example CA)                       TrustRoots(example CA)
  expected server: localhost                   client certificate required
      │                                                   │
      ├── ClientHello ───────────────────────────────────►│
      │◄── server cert + request for client certificate ──┤
      ├── verify server CA + localhost SAN                │
      ├── client certificate + signed proof ─────────────►│
      │                verify client CA, EKU, and proof ──┤
      │                                                   │
      ├── mutually authenticated TLS channel ─────────────┤
      ├── "hello mTLS" ──────────────────────────────────►│
      │◄── "hello mTLS" ──────────────────────────────────┤

  Certificates cross the network during the handshake.
  Private keys never leave their owner.
"#;

struct MutualTlsPki {
    ca_certificate_pem: Vec<u8>,
    server_certificate_pem: Vec<u8>,
    server_private_key_pem: Vec<u8>,
    client_certificate_pem: Vec<u8>,
    client_private_key_pem: Vec<u8>,
}

fn development_pki() -> ExampleResult<MutualTlsPki> {
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "solti-example-ca");
    ca_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    let ca_key = KeyPair::generate()?;
    let ca_certificate = ca_params.self_signed(&ca_key)?;
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut server_params = CertificateParams::new(vec!["localhost".into()])?;
    server_params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = KeyPair::generate()?;
    let server_certificate = server_params.signed_by(&server_key, &issuer)?;

    let mut client_params = CertificateParams::default();
    client_params
        .distinguished_name
        .push(DnType::CommonName, "solti-example-client");
    client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_key = KeyPair::generate()?;
    let client_certificate = client_params.signed_by(&client_key, &issuer)?;

    Ok(MutualTlsPki {
        ca_certificate_pem: ca_certificate.pem().into_bytes(),
        server_certificate_pem: server_certificate.pem().into_bytes(),
        server_private_key_pem: server_key.serialize_pem().into_bytes(),
        client_certificate_pem: client_certificate.pem().into_bytes(),
        client_private_key_pem: client_key.serialize_pem().into_bytes(),
    })
}

async fn serve_once(listener: TcpListener, config: rustls::ServerConfig) -> std::io::Result<()> {
    let (tcp, _) = listener.accept().await?;
    let mut tls = TlsAcceptor::from(Arc::new(config)).accept(tcp).await?;

    let client_certificates = tls.get_ref().1.peer_certificates().map_or(0, <[_]>::len);
    if client_certificates == 0 {
        return Err(io::Error::other("mTLS client presented no certificate"));
    }
    println!(
        "[handshake/server] Required and authenticated a client chain containing {client_certificates} certificate(s)."
    );

    let mut request = vec![0_u8; MESSAGE.len()];
    tls.read_exact(&mut request).await?;
    println!(
        "[data/server] Received {:?}; echoing the same encrypted payload.",
        String::from_utf8_lossy(&request),
    );
    tls.write_all(&request).await?;
    tls.shutdown().await
}

async fn connect_once(address: SocketAddr, config: rustls::ClientConfig) -> io::Result<()> {
    let tcp = TcpStream::connect(address).await?;
    let server_name = ServerName::try_from("localhost").expect("localhost is a valid DNS name");
    let mut tls = TlsConnector::from(Arc::new(config))
        .connect(server_name, tcp)
        .await?;
    println!(
        "[handshake/client] Authenticated localhost, presented its certificate, and proved private-key possession."
    );

    tls.write_all(MESSAGE).await?;
    let mut response = vec![0_u8; MESSAGE.len()];
    tls.read_exact(&mut response).await?;
    println!(
        "[data/client] Received the encrypted echo {:?}.",
        String::from_utf8_lossy(&response),
    );
    assert_eq!(response, MESSAGE);
    tls.shutdown().await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Authenticate both peers when the server must reject clients outside its trust roots."
    );

    let pki = development_pki()?;
    println!("[setup] One CA signs separate server and client certificates.");
    println!("[setup] Each peer keeps its own certificate and private key.");
    println!("[setup] The client trusts the CA for server authentication.");
    println!("[setup] The server trusts the CA and requires client authentication.");

    let server = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_certificate_pem,
        pki.server_private_key_pem,
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_certificate_pem.clone()));
    let client =
        ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_certificate_pem)).with_identity(
            TlsIdentity::from_pem_bytes(pki.client_certificate_pem, pki.client_private_key_pem),
        );

    assert!(server.client_auth_roots().is_some());
    assert!(client.identity().is_some());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::try_join!(
        serve_once(listener, server.into_rustls_config()?),
        connect_once(address, client.into_rustls_config()?),
    )?;
    println!("\nResult: both peers authenticated each other before exchanging application data.");
    Ok(())
}
