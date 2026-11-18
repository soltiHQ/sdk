//! Admission through the bundled core adapter, over both transports.
//!
//! A real `SupervisorApi` routes the body to the first matching runner, whose declaration checks it.
//! A body that runner refuses never reaches the state store; the transport reports the bad field.

#![cfg(all(feature = "core-adapter", feature = "grpc", feature = "http"))]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use solti_api::grpc::wire;
use solti_api::tonic::Code;
use solti_api::{GrpcApi, HttpApi, SupervisorApiAdapter};
use solti_core::SupervisorApi;
use solti_model::{
    TASK_API_VERSION, TASK_KIND, Task, TaskId, TaskManifest, TaskSpec, TaskWorkload, Workload,
    WorkloadBody, WorkloadDecl,
};
use solti_runner::{
    BuildCancellation, BuildContext, BuildScope, RunId, Runner, RunnerError, RunnerRouter,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;
use tower::ServiceExt;

/// The typed body the test runner declares.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)] // only decoded by the admission check
struct Probe {
    target: ProbeTarget,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)] // only decoded by the admission check
struct ProbeTarget {
    port: u16,
}

impl WorkloadBody for Probe {
    const API_VERSION: &'static str = "probes.example.io/v1";
    const KIND: &'static str = "Probe";
}

struct ProbeRunner;

#[solti_runner::async_trait]
impl Runner for ProbeRunner {
    fn name(&self) -> &str {
        "probe"
    }

    fn workload_types(&self) -> Vec<WorkloadDecl> {
        vec![WorkloadDecl::of::<Probe>()]
    }

    async fn build_task(
        &self,
        _task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &BuildCancellation,
        _scope: &mut BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        Ok(TaskFn::arc(|_ctx: TaskContext| async move {
            Ok::<(), TaskError>(())
        }))
    }
}

fn manifest(name: &str, body: &Value) -> TaskManifest {
    let workload =
        Workload::new(Probe::API_VERSION, Probe::KIND, body.clone()).expect("routed workload");
    let spec = TaskSpec::builder("probes", TaskWorkload::Routed(workload), 5_000_u64)
        .build()
        .expect("task spec");
    TaskManifest::new(name, spec).expect("task manifest")
}

fn http_create(name: &str, body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/apis/solti.io/v1/tasks")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&manifest(name, body)).expect("manifest JSON"),
        ))
        .expect("HTTP request")
}

fn grpc_create(name: &str, body: &Value) -> wire::CreateTaskRequest {
    let workload = wire::TaskWorkload {
        api_version: Probe::API_VERSION.into(),
        kind: Probe::KIND.into(),
        spec: Some(wire::task_workload::Spec::Extension(wire::ExtensionTask {
            spec: Some(wire::RawExtension {
                raw: serde_json::to_vec(body).expect("body JSON"),
            }),
        })),
    };
    wire::CreateTaskRequest {
        manifest: Some(wire::TaskManifest {
            api_version: TASK_API_VERSION.into(),
            kind: TASK_KIND.into(),
            metadata: Some(wire::TaskManifestMeta {
                name: name.into(),
                ..Default::default()
            }),
            spec: Some(wire::TaskSpec {
                slot: "probes".into(),
                workload: Some(workload),
                timeout_ms: 5_000,
                restart: wire::RestartPolicy::Never as i32,
                backoff: Some(wire::BackoffPolicy {
                    jitter: wire::JitterPolicy::Full as i32,
                    first_ms: 1_000,
                    max_ms: 10_000,
                    factor: 2.0,
                }),
                admission: wire::AdmissionPolicy::DropIfRunning as i32,
                ..Default::default()
            }),
        }),
    }
}

#[tokio::test]
async fn adapter_refuses_an_invalid_body_with_invalid_request() {
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(ProbeRunner))
        .expect("the probe declaration is valid");
    let supervisor = Arc::new(
        SupervisorApi::builder(router)
            .start()
            .await
            .expect("core starts"),
    );
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
    let invalid = json!({ "target": { "port": "eighty" } });
    let valid = json!({ "target": { "port": 80 } });

    let app = HttpApi::new(Arc::clone(&handler)).router();
    let refused = app
        .clone()
        .oneshot(http_create("probe-http-invalid", &invalid))
        .await
        .expect("HTTP router answers");
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let bytes = refused
        .into_body()
        .collect()
        .await
        .expect("HTTP body")
        .to_bytes();
    let status: Value = serde_json::from_slice(&bytes).expect("Status JSON");
    assert_eq!(status["reason"], "BadRequest");
    let message = status["message"].as_str().expect("Status message");
    assert!(
        message.contains("spec.target.port"),
        "the HTTP message must name the bad field, got: {message}"
    );
    let admitted = app
        .oneshot(http_create("probe-http-valid", &valid))
        .await
        .expect("HTTP router answers");
    assert_eq!(admitted.status(), StatusCode::CREATED);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let address = listener.local_addr().expect("listener address");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(
        solti_api::tonic::transport::Server::builder()
            .add_service(GrpcApi::new(Arc::clone(&handler)).server())
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            }),
    );
    let mut client = wire::TaskServiceClient::connect(format!("http://{address}"))
        .await
        .expect("connect the generated client");
    let refused = client
        .create_task(grpc_create("probe-grpc-invalid", &invalid))
        .await
        .expect_err("the probe runner refuses the body");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused.message().contains("spec.target.port"),
        "the gRPC message must name the bad field, got: {}",
        refused.message()
    );
    client
        .create_task(grpc_create("probe-grpc-valid", &valid))
        .await
        .expect("a valid body is admitted over gRPC");

    for name in ["probe-http-invalid", "probe-grpc-invalid"] {
        let name = TaskId::new(name).expect("task name");
        assert!(
            supervisor.get_task(&name).is_none(),
            "a refused body must not be committed: {name}"
        );
    }

    drop(client);
    shutdown_tx.send(()).expect("the gRPC server is running");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the gRPC server stops within the bound")
        .expect("the gRPC server task does not panic")
        .expect("the gRPC server exits cleanly");
    supervisor.shutdown().await.expect("core shuts down");
}
