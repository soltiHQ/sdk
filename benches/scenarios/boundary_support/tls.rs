//! Reusable loopback endpoint with a fresh, fully authenticated TLS connection per session.

use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use solti_benches::fixtures::bounded;
use solti_tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

pub type ClientConnection = tokio_rustls::client::TlsStream<TcpStream>;

pub struct Pki {
    ca: Vec<u8>,
    server_certificate: Vec<u8>,
    server_key: Vec<u8>,
    client_certificate: Vec<u8>,
    client_key: Vec<u8>,
}

impl Pki {
    pub fn generate() -> Self {
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().expect("CA key");
        let ca = ca_params.self_signed(&ca_key).expect("CA certificate");
        let issuer = Issuer::from_params(&ca_params, &ca_key);

        let mut server_params = CertificateParams::new(vec!["localhost".into()]).unwrap();
        server_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_key = KeyPair::generate().unwrap();
        let server = server_params.signed_by(&server_key, &issuer).unwrap();
        let mut client_params = CertificateParams::default();
        client_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_key = KeyPair::generate().unwrap();
        let client = client_params.signed_by(&client_key, &issuer).unwrap();

        Self {
            ca: ca.pem().into_bytes(),
            server_certificate: server.pem().into_bytes(),
            server_key: server_key.serialize_pem().into_bytes(),
            client_certificate: client.pem().into_bytes(),
            client_key: client_key.serialize_pem().into_bytes(),
        }
    }

    pub fn sources(&self, mutual: bool) -> (ClientTlsConfig, ServerTlsConfig) {
        let mut client = ClientTlsConfig::new(TrustRoots::from_pem_bytes(self.ca.clone()));
        let mut server = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
            self.server_certificate.clone(),
            self.server_key.clone(),
        ));
        if mutual {
            client = client.with_identity(TlsIdentity::from_pem_bytes(
                self.client_certificate.clone(),
                self.client_key.clone(),
            ));
            server = server.require_client_auth(TrustRoots::from_pem_bytes(self.ca.clone()));
        }
        (client, server)
    }

    pub fn configurations(
        &self,
        mutual: bool,
    ) -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
        let (client, server) = self.sources(mutual);
        let mut client = client
            .into_rustls_config()
            .expect("client TLS configuration");
        client.resumption = rustls::client::Resumption::disabled();
        let server = server
            .into_rustls_config()
            .expect("server TLS configuration");
        (Arc::new(client), Arc::new(server))
    }
}

/// Retains one listening port across Criterion warm-up, calibration, and samples.
/// Accepted connections are never reused by the cold-connection scenario.
pub struct EchoEndpoint {
    listener: Arc<TcpListener>,
    address: SocketAddr,
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    mutual: bool,
}

impl EchoEndpoint {
    pub async fn bind(
        client: Arc<rustls::ClientConfig>,
        server: Arc<rustls::ServerConfig>,
        mutual: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TLS fixture listener");
        let address = listener.local_addr().unwrap();
        Self {
            listener: Arc::new(listener),
            address,
            client,
            server,
            mutual,
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn echo(&self, bytes: usize, exchanges: u64) -> EchoSession {
        let (close, closing) = oneshot::channel();
        let listener = Arc::clone(&self.listener);
        let server = Arc::clone(&self.server);
        let mutual = self.mutual;
        let task = tokio::spawn(async move {
            let (tcp, peer) = bounded(listener.accept())
                .await
                .expect("TLS fixture accept");
            tcp.set_nodelay(true).unwrap();
            let mut tls = bounded(TlsAcceptor::from(server).accept(tcp))
                .await
                .expect("server TLS handshake");
            let handshake = full_handshake(tls.get_ref().1.handshake_kind());
            let authenticated = tls
                .get_ref()
                .1
                .peer_certificates()
                .is_some_and(|certificates| !certificates.is_empty());
            assert_eq!(authenticated, mutual, "unexpected client authentication");
            let mut payload = vec![0_u8; bytes];
            for _ in 0..exchanges {
                bounded(tls.read_exact(&mut payload)).await.unwrap();
                bounded(tls.write_all(&payload)).await.unwrap();
                bounded(tls.flush()).await.unwrap();
            }

            bounded(closing)
                .await
                .expect("client finished its timed exchanges");
            bounded(tls.shutdown())
                .await
                .expect("server TLS/TCP write shutdown");
            let mut trailing = [0_u8; 1];
            assert_eq!(
                bounded(tls.read(&mut trailing))
                    .await
                    .expect("client close_notify"),
                0,
                "unexpected client data after the final exchange"
            );
            assert_eq!(
                bounded(tls.get_mut().0.read(&mut trailing))
                    .await
                    .expect("client TCP FIN"),
                0,
                "unexpected client transport bytes after close_notify"
            );
            EchoReport {
                peer,
                handshake,
                authenticated,
            }
        });
        EchoSession {
            task: Some(task),
            close: Some(close),
            mutual,
        }
    }

    pub async fn connect(&self) -> ClientConnection {
        let socket = bounded(TcpStream::connect(self.address()))
            .await
            .expect("TLS fixture TCP connect");
        socket.set_nodelay(true).unwrap();
        let tls = bounded(
            TlsConnector::from(Arc::clone(&self.client))
                .connect("localhost".try_into().unwrap(), socket),
        )
        .await
        .expect("client TLS handshake");
        full_handshake(tls.get_ref().1.handshake_kind());
        tls
    }
}

fn full_handshake(handshake: Option<rustls::HandshakeKind>) -> rustls::HandshakeKind {
    let handshake = handshake.expect("completed TLS handshake");
    assert!(
        matches!(
            handshake,
            rustls::HandshakeKind::Full | rustls::HandshakeKind::FullWithHelloRetryRequest
        ),
        "cold TLS fixture unexpectedly resumed a session"
    );
    handshake
}

pub async fn exchange(connection: &mut ClientConnection, payload: &[u8], received: &mut [u8]) {
    bounded(connection.write_all(payload)).await.unwrap();
    bounded(connection.flush()).await.unwrap();
    bounded(connection.read_exact(received)).await.unwrap();
    assert_eq!(received, payload);
}

pub struct EchoReport {
    pub peer: SocketAddr,
    pub handshake: rustls::HandshakeKind,
    pub authenticated: bool,
}

pub struct EchoSession {
    task: Option<JoinHandle<EchoReport>>,
    close: Option<oneshot::Sender<()>>,
    mutual: bool,
}

impl EchoSession {
    /// Runs outside the measured interval. The server closes first, keeping
    /// TIME_WAIT on the single listener port instead of consuming fresh ports.
    pub async fn finish(mut self, mut connection: ClientConnection) -> EchoReport {
        let local = connection.get_ref().0.local_addr().unwrap();
        self.close
            .take()
            .unwrap()
            .send(())
            .expect("TLS echo server is still awaiting teardown");
        let mut trailing = [0_u8; 1];
        assert_eq!(
            bounded(connection.read(&mut trailing))
                .await
                .expect("server close_notify"),
            0,
            "unexpected server data after the final echo"
        );
        // TLS EOF observes close_notify, which can arrive before the TCP FIN.
        // Observe that FIN before closing the client's write half.
        assert_eq!(
            bounded(connection.get_mut().0.read(&mut trailing))
                .await
                .expect("server TCP FIN"),
            0,
            "unexpected server transport bytes after close_notify"
        );
        bounded(connection.shutdown())
            .await
            .expect("client TLS/TCP write shutdown");
        drop(connection);
        let report = bounded(self.task.as_mut().unwrap())
            .await
            .expect("TLS echo server joined");
        self.task.take();
        assert_eq!(
            report.peer, local,
            "echo server accepted another connection"
        );
        assert_eq!(report.authenticated, self.mutual);
        full_handshake(Some(report.handshake));
        report
    }
}

impl Drop for EchoSession {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
