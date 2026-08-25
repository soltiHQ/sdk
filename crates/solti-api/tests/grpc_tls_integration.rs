//! End-to-end Tonic TLS and mTLS through the Solti server adapter.

#![cfg(feature = "grpc-tls")]

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    SanType,
};
use solti_api::grpc::wire::{GetTaskRequest, TaskServiceClient};
use solti_api::tonic::transport::{
    Certificate, Channel, ClientTlsConfig as TonicClientTlsConfig, Endpoint, Identity,
};
use solti_api::{
    ApiError, ApiHandler, GrpcApi, OutputEventStream, TaskWatchEventStream, to_tonic_server_tls,
};
use solti_model::{
    Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRunPage, TaskRunQuery,
    WritePreconditions,
};
use solti_tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;

struct Pki {
    ca_certificate_pem: Vec<u8>,
    server_certificate_pem: Vec<u8>,
    server_private_key_pem: Vec<u8>,
    client_certificate_pem: Vec<u8>,
    client_private_key_pem: Vec<u8>,
}

fn make_pki() -> Pki {
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "solti-tonic-test-ca");
    let ca_key = KeyPair::generate().expect("generate CA key");
    let ca_certificate = ca_params.self_signed(&ca_key).expect("sign CA certificate");
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let mut server_params = CertificateParams::default();
    server_params.subject_alt_names = vec![SanType::IpAddress(IpAddr::from([127, 0, 0, 1]))];
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params
        .distinguished_name
        .push(DnType::CommonName, "solti-tonic-test-server");
    let server_key = KeyPair::generate().expect("generate server key");
    let server_certificate = server_params
        .signed_by(&server_key, &issuer)
        .expect("sign server certificate");

    let mut client_params = CertificateParams::default();
    client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    client_params
        .distinguished_name
        .push(DnType::CommonName, "solti-tonic-test-client");
    let client_key = KeyPair::generate().expect("generate client key");
    let client_certificate = client_params
        .signed_by(&client_key, &issuer)
        .expect("sign client certificate");

    Pki {
        ca_certificate_pem: ca_certificate.pem().into_bytes(),
        server_certificate_pem: server_certificate.pem().into_bytes(),
        server_private_key_pem: server_key.serialize_pem().into_bytes(),
        client_certificate_pem: client_certificate.pem().into_bytes(),
        client_private_key_pem: client_key.serialize_pem().into_bytes(),
    }
}

#[derive(Default)]
struct TlsHandler {
    get_calls: AtomicUsize,
}

#[async_trait]
impl ApiHandler for TlsHandler {
    async fn create_task(&self, _manifest: TaskManifest) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn apply_task(
        &self,
        _manifest: TaskManifest,
        _preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn get_task(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn watch_tasks(
        &self,
        _filter: TaskFilter,
        _resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn query_task_runs(
        &self,
        _id: &TaskId,
        _query: TaskRunQuery,
    ) -> Result<TaskRunPage, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn cancel_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn delete_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn stream_task_logs(
        &self,
        _id: &TaskId,
        _task_uid: &solti_model::Uid,
    ) -> Result<OutputEventStream, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }
}

async fn spawn_server(
    pki: &Pki,
    handler: Arc<TlsHandler>,
    require_client_auth: bool,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), solti_api::tonic::transport::Error>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local Tonic listener");
    let address = listener.local_addr().expect("read Tonic listener address");
    let incoming = TcpListenerStream::new(listener);
    let mut server_tls = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_certificate_pem.clone(),
        pki.server_private_key_pem.clone(),
    ));
    if require_client_auth {
        server_tls = server_tls
            .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_certificate_pem.clone()));
    }
    let server_tls = to_tonic_server_tls(server_tls).expect("build Solti Tonic TLS adapter");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        solti_api::tonic::transport::Server::builder()
            .tls_config(server_tls)?
            .add_service(GrpcApi::new(handler).server())
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    (address, shutdown_tx, server)
}

async fn connect_client(
    address: std::net::SocketAddr,
    pki: &Pki,
    with_identity: bool,
) -> Result<TaskServiceClient<Channel>, solti_api::tonic::transport::Error> {
    let mut config =
        ClientTlsConfig::new(TrustRoots::from_pem_bytes(pki.ca_certificate_pem.clone()));
    if with_identity {
        config = config.with_identity(TlsIdentity::from_pem_bytes(
            pki.client_certificate_pem.clone(),
            pki.client_private_key_pem.clone(),
        ));
    }
    let loaded = config.load().expect("validate Solti client TLS material");
    let mut tls = TonicClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(loaded.server_roots_pem()))
        .domain_name("127.0.0.1");
    if let Some(identity) = loaded.identity() {
        tls = tls.identity(Identity::from_pem(
            identity.certificate_chain_pem(),
            identity.expose_private_key_pem(),
        ));
    }
    let channel = Endpoint::from_shared(format!("https://{address}"))?
        .connect_timeout(Duration::from_secs(1))
        .tls_config(tls)?
        .connect()
        .await?;
    Ok(TaskServiceClient::new(channel))
}

async fn stop_server(
    shutdown_tx: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Result<(), solti_api::tonic::transport::Error>>,
) {
    shutdown_tx.send(()).expect("Tonic server is still running");
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("Tonic server shuts down within the bound")
        .expect("Tonic server task does not panic")
        .expect("Tonic server exits cleanly");
}

async fn get_missing_task(client: &mut TaskServiceClient<Channel>) {
    let status = tokio::time::timeout(
        Duration::from_secs(2),
        client.get_task(GetTaskRequest {
            name: "tls-missing-task".into(),
        }),
    )
    .await
    .expect("gRPC request finishes within the bound")
    .expect_err("the test handler reports a missing task");
    assert_eq!(status.code(), solti_api::tonic::Code::NotFound);
}

#[tokio::test]
async fn tonic_tls_round_trip_uses_the_solti_server_adapter() {
    let pki = make_pki();
    let handler = Arc::new(TlsHandler::default());
    let (address, shutdown_tx, server) = spawn_server(&pki, Arc::clone(&handler), false).await;
    let mut client =
        tokio::time::timeout(Duration::from_secs(2), connect_client(address, &pki, false))
            .await
            .expect("TLS connect finishes within the bound")
            .expect("TLS client connects");

    get_missing_task(&mut client).await;
    assert_eq!(handler.get_calls.load(Ordering::SeqCst), 1);

    drop(client);
    stop_server(shutdown_tx, server).await;
}

#[tokio::test]
async fn tonic_mtls_rejects_anonymous_client_and_accepts_solti_identity() {
    let pki = make_pki();
    let handler = Arc::new(TlsHandler::default());
    let (address, shutdown_tx, server) = spawn_server(&pki, Arc::clone(&handler), true).await;

    let anonymous =
        tokio::time::timeout(Duration::from_secs(2), connect_client(address, &pki, false))
            .await
            .expect("anonymous mTLS connect finishes within the bound");
    match anonymous {
        Err(_) => {}
        Ok(mut client) => {
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                client.get_task(GetTaskRequest {
                    name: "must-not-reach-handler".into(),
                }),
            )
            .await;
            assert!(
                !matches!(result, Ok(Ok(_))),
                "mTLS server accepted an anonymous client"
            );
        }
    }
    assert_eq!(handler.get_calls.load(Ordering::SeqCst), 0);

    let mut authenticated =
        tokio::time::timeout(Duration::from_secs(2), connect_client(address, &pki, true))
            .await
            .expect("authenticated mTLS connect finishes within the bound")
            .expect("mTLS client connects with its identity");
    get_missing_task(&mut authenticated).await;
    assert_eq!(handler.get_calls.load(Ordering::SeqCst), 1);

    drop(authenticated);
    stop_server(shutdown_tx, server).await;
}
