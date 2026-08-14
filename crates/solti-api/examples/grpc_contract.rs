//! # gRPC task contract
//!
//! `GrpcApi` exposes one `ApiHandler` through the generated tonic service.
//! Consumers use the generated client and versioned protobuf messages.
//!
//! This example shows:
//!
//! - a real local gRPC server and generated client;
//! - bearer metadata authentication;
//! - one unary `ListTasks` call;
//! - one server-streaming `StreamTaskLogs` call;
//! - protobuf task and output oneof values;
//! - graceful server shutdown without timing sleeps.
//!
//! Run with `cargo run -p solti-api --example grpc_contract --features grpc`.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::json;
use solti_api::grpc::wire::{
    ListTasksRequest, OutputStreamKind, StreamTaskLogsRequest, TaskServiceClient,
    stream_task_logs_response,
};
use solti_api::tonic::{Code, Request};
use solti_api::{
    ApiError, ApiHandler, GRPC_API_SERVICE, GrpcApi, OutputEventStream, TaskWatchEventStream,
};
use solti_model::{
    ExtensionWorkload, OutputChunk, OutputEvent, StreamKind, Task, TaskFilter, TaskId,
    TaskManifest, TaskPage, TaskQuery, TaskRunPage, TaskRunQuery, TaskSpec, TaskWatchEvent,
    TaskWorkload, Token, WritePreconditions,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-api: gRPC transport

  generated TaskServiceClient
            │ protobuf request + authorization metadata
            ▼
  tonic TaskService (solti.task.v1)
            ├──► authentication + message limit + conversion
            └──► ApiHandler ──► application backend
                         ├──► unary ListTasksResponse
                         └──► StreamTaskLogsResponse oneof stream

  The wire package and service identify the API version.
  The handler receives the same solti-model values used by the HTTP transport.
"#;

struct SnapshotHandler {
    task: Task,
}

#[async_trait]
impl ApiHandler for SnapshotHandler {
    async fn create_task(&self, _manifest: TaskManifest) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed(
            "the teaching backend is read-only".into(),
        ))
    }

    async fn apply_task(
        &self,
        _manifest: TaskManifest,
        _preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed(
            "the teaching backend is read-only".into(),
        ))
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok((id == self.task.name()).then(|| self.task.clone()))
    }

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        if query.continuation().is_some() {
            return Err(ApiError::MethodNotAllowed(
                "the teaching backend has one fixed snapshot".into(),
            ));
        }
        let items = if query.limit() > 0 && query.matches(&self.task) {
            vec![self.task.clone()]
        } else {
            Vec::new()
        };
        Ok(TaskPage {
            items,
            resource_version: "snapshot:1".into(),
            continuation: None,
            remaining_item_count: 0,
        })
    }

    async fn watch_tasks(
        &self,
        filter: TaskFilter,
        _resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        let events = filter
            .matches(&self.task)
            .then(|| Ok(TaskWatchEvent::Added(self.task.clone())))
            .into_iter();
        Ok(Box::pin(tokio_stream::iter(events)))
    }

    async fn query_task_runs(
        &self,
        id: &TaskId,
        query: TaskRunQuery,
    ) -> Result<TaskRunPage, ApiError> {
        if id != self.task.name() {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        if query.continuation().is_some() {
            return Err(ApiError::MethodNotAllowed(
                "the teaching backend has one fixed run snapshot".into(),
            ));
        }
        Ok(TaskRunPage {
            items: Vec::new(),
            task: id.clone(),
            task_uid: self.task.metadata().uid().clone(),
            resource_version: "runs-snapshot:1".into(),
            continuation: None,
            remaining_item_count: 0,
        })
    }

    async fn delete_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        Err(ApiError::MethodNotAllowed(
            "the teaching backend is read-only".into(),
        ))
    }

    async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError> {
        if id != self.task.name() {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        let events = vec![
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1_000),
            },
            OutputEvent::Chunk(OutputChunk {
                generation: 1,
                attempt: 1,
                stream: StreamKind::Stdout,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1_100),
                line: Bytes::from_static(b"resized cover.png"),
                truncated: false,
            }),
            OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(0),
                finished_at: UNIX_EPOCH + Duration::from_millis(1_200),
            },
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn fixture_task() -> ExampleResult<Task> {
    let workload = TaskWorkload::Extension(ExtensionWorkload::new(
        "media.example.io/v1",
        "ImageResize",
        json!({
            "source": "cover.png",
            "width": 1280
        }),
    )?);
    let spec = TaskSpec::builder("image-processing", workload, 30_000_u64).build()?;
    let mut task = Task::new("resize-cover", spec)?;
    task.set_resource_version("snapshot:1")?;
    Ok(task)
}

fn authenticated<T>(message: T) -> ExampleResult<Request<T>> {
    let mut request = Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", "Bearer example-token".parse()?);
    Ok(request)
}

fn list_request() -> ListTasksRequest {
    ListTasksRequest {
        slot: Some("image-processing".into()),
        phases: Vec::new(),
        limit: 10,
        label_selector: String::new(),
        r#continue: String::new(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Use the generated client against a real local service and inspect unary and streaming protobuf responses."
    );

    let handler = Arc::new(SnapshotHandler {
        task: fixture_task()?,
    });
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        solti_api::tonic::transport::Server::builder()
            .add_service(
                GrpcApi::new(handler)
                    .with_auth(Token::new("example-token").expect("valid teaching token"))
                    .server(),
            )
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    println!("[server] Listening on {address}; service={GRPC_API_SERVICE}.");

    let endpoint = format!("http://{address}");
    let mut client = TaskServiceClient::connect(endpoint).await?;
    let unauthenticated = client
        .list_tasks(list_request())
        .await
        .expect_err("missing metadata must be rejected");
    println!(
        "[auth] Missing authorization metadata: code={:?}.",
        unauthenticated.code(),
    );
    assert_eq!(unauthenticated.code(), Code::Unauthenticated);

    let page = client
        .list_tasks(authenticated(list_request())?)
        .await?
        .into_inner();
    let task = page.tasks.first().ok_or("ListTasks returned no task")?;
    let metadata = task
        .metadata
        .as_ref()
        .ok_or("Task response has no metadata")?;
    let workload = task
        .spec
        .as_ref()
        .and_then(|spec| spec.workload.as_ref())
        .ok_or("Task response has no workload")?;
    println!(
        "[list] resourceVersion={}, tasks={}, name={}, workload={}/{}.",
        page.resource_version,
        page.tasks.len(),
        metadata.name,
        workload.api_version,
        workload.kind,
    );
    assert_eq!(metadata.name, "resize-cover");

    let mut logs = client
        .stream_task_logs(authenticated(StreamTaskLogsRequest {
            name: "resize-cover".into(),
        })?)
        .await?
        .into_inner();
    let mut event_count = 0;
    while let Some(event) = logs.message().await? {
        let kind = event.kind.ok_or("log response has no oneof value")?;
        match kind {
            stream_task_logs_response::Kind::RunStarted(started) => {
                println!(
                    "[logs] RunStarted: generation={}, attempt={}.",
                    started.generation, started.attempt,
                );
            }
            stream_task_logs_response::Kind::Chunk(chunk) => {
                let stream = OutputStreamKind::try_from(chunk.stream)
                    .map(|value| value.as_str_name())
                    .unwrap_or("OUTPUT_STREAM_KIND_UNKNOWN");
                println!(
                    "[logs] Chunk: generation={}, attempt={}, stream={}, seq={}, line={:?}.",
                    chunk.generation,
                    chunk.attempt,
                    stream,
                    chunk.seq,
                    String::from_utf8_lossy(&chunk.line),
                );
            }
            stream_task_logs_response::Kind::RunFinished(finished) => {
                println!(
                    "[logs] RunFinished: generation={}, attempt={}, exitCode={:?}.",
                    finished.generation, finished.attempt, finished.exit_code,
                );
            }
            stream_task_logs_response::Kind::Lagged(lagged) => {
                println!("[logs] Lagged: skipped={}.", lagged.skipped);
            }
        }
        event_count += 1;
    }
    assert_eq!(event_count, 3);

    let _ = shutdown_tx.send(());
    server.await??;
    println!(
        "\nResult: the generated client authenticated, decoded one Task page, consumed three typed output events, and shut the server down cleanly."
    );
    Ok(())
}
