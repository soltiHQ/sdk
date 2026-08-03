//! # TLS round trip
//!
//! A server presents its identity.
//! A client verifies that identity against explicit trust roots.
//! The expected server name is checked during the handshake.
//!
//! This example shows the complete `rustls` path:
//!
//! - build server and client configurations from in-memory PEM;
//! - set ALPN at the transport boundary;
//! - connect with the expected server name;
//! - exchange one message over the encrypted stream;
//! - wait for both peer futures before exit.
//!
//! The PKI is generated only to keep the example self-contained.
//! Production applications normally load certificates issued outside the process.
//!
//! Run with `cargo run -p solti-tls --example tls_round_trip`.

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

const ALPN: &[u8] = b"solti-example/1";
const MESSAGE: &[u8] = b"hello TLS";

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-tls: one-way TLS

  client                                              server
  TrustRoots(example CA)                       TlsIdentity(cert + key)
  expected name: localhost                     certificate SAN: localhost
      │                                                    │
      ├── ClientHello + ALPN ─────────────────────────────►│
      │◄── server certificate + signed proof + ALPN ───────┤
      ├── verify CA, localhost SAN, and signed proof       │
      │                                                    │
      ├── encrypted TLS channel ───────────────────────────┤
      ├── "hello TLS" ────────────────────────────────────►│
      │◄── "hello TLS" ────────────────────────────────────┤

  The client authenticates the server.
  The server does not request a client certificate.
"#;

struct ServerPki {
    ca_certificate_pem: Vec<u8>,
    certificate_pem: Vec<u8>,
    private_key_pem: Vec<u8>,
}

fn development_pki() -> ExampleResult<ServerPki> {
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

    Ok(ServerPki {
        ca_certificate_pem: ca_certificate.pem().into_bytes(),
        certificate_pem: server_certificate.pem().into_bytes(),
        private_key_pem: server_key.serialize_pem().into_bytes(),
    })
}

async fn serve_once(listener: TcpListener, config: rustls::ServerConfig) -> std::io::Result<()> {
    let (tcp, _) = listener.accept().await?;
    let mut tls = TlsAcceptor::from(Arc::new(config)).accept(tcp).await?;
    let negotiated_alpn = tls
        .get_ref()
        .1
        .alpn_protocol()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    println!(
        "[handshake/server] Presented the localhost certificate and proved private-key possession; ALPN={negotiated_alpn}."
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
    let negotiated_alpn = tls
        .get_ref()
        .1
        .alpn_protocol()
        .map(String::from_utf8_lossy)
        .unwrap_or_default();
    println!(
        "[handshake/client] Trusted the signing CA, matched localhost, and accepted ALPN={negotiated_alpn}."
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
        "[purpose] Authenticate the server and encrypt traffic without requiring a client certificate."
    );

    let pki = development_pki()?;
    println!("[setup] The example CA signs one server certificate for localhost.");
    println!("[setup] The server owns that certificate and its private key.");
    println!("[setup] The client trusts the CA and expects the name localhost.");
    println!("[setup] The client has no identity; only the server is authenticated.");

    let mut server_config = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.certificate_pem,
        pki.private_key_pem,
    ))
    .into_rustls_config()?;
    let mut client_config =
        ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_certificate_pem))
            .into_rustls_config()?;

    assert!(server_config.alpn_protocols.is_empty());
    assert!(client_config.alpn_protocols.is_empty());
    server_config.alpn_protocols = vec![ALPN.to_vec()];
    client_config.alpn_protocols = vec![ALPN.to_vec()];
    println!("[transport] The adapter configures ALPN solti-example/1 on both peers.");

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::try_join!(
        serve_once(listener, server_config),
        connect_once(address, client_config),
    )?;
    println!("\nResult: the client authenticated the server before exchanging application data.");
    Ok(())
}
