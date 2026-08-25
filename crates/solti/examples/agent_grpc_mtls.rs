//! # mTLS gRPC agent
//!
//! A real gRPC Task API requires both peers to authenticate before protobuf exchange.
//! The server then delegates the accepted call to the same core supervisor used in-process.
//!
//! This example shows:
//!
//! - an ephemeral teaching CA;
//! - separate server and client identities;
//! - mandatory client certificate authentication;
//! - conversion from `solti-tls` to tonic server TLS;
//! - rejection of a client without an identity;
//! - one authenticated generated-client call backed by core.
//!
//! ```text
//! teaching CA
//!    ├──► server identity ──► ServerTlsConfig ──► tonic gRPC server
//!    └──► client identity ──► tonic client TLS ───────────┐
//!                                                         │ mTLS
//! generated client ───────────────────────────────────────┤
//!                                                         ▼
//!                                        SupervisorApiAdapter ──► core
//! ```
//!
//! The generated PKI keeps the example self-contained.
//! Production binaries normally load externally issued certificates.
//!
//! Run with `cargo run -p solti --example agent_grpc_mtls --features api-core-adapter,api-grpc-tls,exec-subprocess`.

use std::{env, io, sync::Arc, time::Duration};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use solti::{
    api::{
        GrpcApi, SupervisorApiAdapter,
        grpc::wire::{ListTasksRequest, TaskServiceClient},
        to_tonic_server_tls,
        tonic::transport::{
            Certificate, ClientTlsConfig as TonicClientTls, Endpoint, Identity, Server,
        },
    },
    core::SupervisorApi,
    exec::subprocess::register_subprocess_runner,
    model::{
        Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest, TaskSpec,
        TaskWorkload,
    },
    runner::RunnerRouter,
    tls::{ServerTlsConfig, TlsIdentity, TrustRoots},
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;

const CHILD_MODE: &str = "--mtls-grpc-agent-child";

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

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
        .push(DnType::CommonName, "solti-umbrella-example-ca");
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
        .push(DnType::CommonName, "solti-umbrella-example-client");
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    if env::args().nth(1).as_deref() == Some(CHILD_MODE) {
        println!("hello from the mTLS gRPC agent subprocess");
        return Ok(());
    }

    println!(
        r#"
solti: mutually authenticated gRPC task agent

  client cert ──► mTLS handshake ◄── server cert
                         │
                         ▼
               generated gRPC API ──► adapter ──► core ──► subprocess
"#
    );
    println!("[purpose] Require a trusted client identity before exposing supervised task state.");

    let pki = development_pki()?;
    println!("[pki] Generated one CA and separate server/client identities.");

    let mut router = RunnerRouter::new();
    let subprocess_runner = register_subprocess_runner(&mut router, "default")?;
    let supervisor = Arc::new(SupervisorApi::builder(router).start().await?);
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: current_executable()?,
            args: vec![CHILD_MODE.into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("mtls-example", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    supervisor
        .create_task(TaskManifest::new("mtls-subprocess", spec)?)
        .await?;

    let server_tls = ServerTlsConfig::new(TlsIdentity::from_pem_bytes(
        pki.server_certificate_pem,
        pki.server_private_key_pem,
    ))
    .require_client_auth(TrustRoots::from_pem_bytes(pki.ca_certificate_pem.clone()));
    let tonic_server_tls = to_tonic_server_tls(server_tls)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .tls_config(tonic_server_tls)?
            .add_service(GrpcApi::new(handler).server())
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let endpoint = format!("https://localhost:{}", address.port());
    println!("[server] Listening at {endpoint}; client certificates are mandatory.");

    let anonymous_tls = TonicClientTls::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(pki.ca_certificate_pem.clone()));
    let anonymous_channel = Endpoint::from_shared(endpoint.clone())?
        .tls_config(anonymous_tls)?
        .connect()
        .await?;
    let anonymous = TaskServiceClient::new(anonymous_channel)
        .list_tasks(list_request())
        .await;
    assert!(anonymous.is_err());
    println!("[auth] A client without a certificate was rejected before its RPC reached the API.");

    let client_tls = TonicClientTls::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(pki.ca_certificate_pem))
        .identity(Identity::from_pem(
            pki.client_certificate_pem,
            pki.client_private_key_pem,
        ));
    let channel = Endpoint::from_shared(endpoint)?
        .tls_config(client_tls)?
        .connect()
        .await?;
    let mut client = TaskServiceClient::new(channel);
    let page = client.list_tasks(list_request()).await?.into_inner();
    let name = page
        .tasks
        .first()
        .and_then(|task| task.metadata.as_ref())
        .map(|metadata| metadata.name.as_str())
        .ok_or_else(|| io::Error::other("authenticated ListTasks returned no task"))?;
    println!("[client] Authenticated both peers and received task={name}.");
    assert_eq!(name, "mtls-subprocess");

    let _ = shutdown_tx.send(());
    server.await??;
    supervisor.shutdown().await?;
    subprocess_runner.shutdown(Duration::from_secs(5)).await?;
    println!(
        "\nResult: only the client trusted by the configured CA reached the public task handler."
    );
    Ok(())
}

fn list_request() -> ListTasksRequest {
    ListTasksRequest {
        slot: Some("mtls-example".into()),
        phases: Vec::new(),
        limit: 10,
        label_selector: String::new(),
        r#continue: String::new(),
    }
}

fn current_executable() -> ExampleResult<String> {
    env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "example path is not UTF-8").into())
}
