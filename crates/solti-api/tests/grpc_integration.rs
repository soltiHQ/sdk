//! End-to-end gRPC contract over a real loopback TCP socket.

#![cfg(feature = "grpc")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use solti_api::grpc::wire::{
    CancelTaskRequest, GetTaskRequest, TaskServiceClient, TaskWatchEventType, WatchTasksRequest,
    WritePreconditions as WireWritePreconditions,
};
use solti_api::tonic::{Code, Request};
use solti_api::{
    ApiError, ApiHandler, GrpcApi, MAX_REQUEST_BYTES, OutputEventStream, TaskWatchEventStream,
};
use solti_model::{
    ExtensionWorkload, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRunPage,
    TaskRunQuery, TaskSpec, TaskWatchEvent, TaskWorkload, Token, WritePreconditions,
};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;

struct SocketHandler {
    task: Task,
    get_calls: AtomicUsize,
    last_cancel_preconditions: Mutex<Option<WritePreconditions>>,
}

#[async_trait]
impl ApiHandler for SocketHandler {
    async fn create_task(&self, _manifest: TaskManifest) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed("read-only test backend".into()))
    }

    async fn apply_task(
        &self,
        _manifest: TaskManifest,
        _preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed("read-only test backend".into()))
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, ApiError> {
        self.get_calls.fetch_add(1, Ordering::SeqCst);
        Ok((id == self.task.name()).then(|| self.task.clone()))
    }

    async fn query_tasks(&self, _query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }

    async fn watch_tasks(
        &self,
        _filter: TaskFilter,
        _resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        Ok(Box::pin(tokio_stream::iter([
            Ok(TaskWatchEvent::Added(self.task.clone())),
            Err(ApiError::ResourceVersionExpired(
                "watch position is no longer retained".into(),
            )),
        ])))
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
        id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        if id != self.task.name() {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        *self
            .last_cancel_preconditions
            .lock()
            .expect("cancel precondition lock is not poisoned") = Some(preconditions);
        Ok(())
    }

    async fn delete_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        Err(ApiError::MethodNotAllowed("read-only test backend".into()))
    }

    async fn stream_task_logs(&self, _id: &TaskId) -> Result<OutputEventStream, ApiError> {
        Err(ApiError::MethodNotAllowed("not used by this test".into()))
    }
}

fn fixture_task() -> Task {
    let workload = TaskWorkload::Extension(
        ExtensionWorkload::new(
            "workloads.example.io/v1",
            "SocketTest",
            json!({ "value": 1 }),
        )
        .expect("valid extension workload"),
    );
    let spec = TaskSpec::builder("socket-tests", workload, 30_000_u64)
        .build()
        .expect("valid task spec");
    let mut task = Task::new("socket-task", spec).expect("valid task");
    task.set_resource_version("socket:1")
        .expect("valid resource version");
    task
}

fn authenticated<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        "Bearer socket-test-token"
            .parse()
            .expect("valid authorization metadata"),
    );
    request
}

#[tokio::test]
async fn generated_client_observes_the_real_grpc_contract_over_tcp() {
    let handler = Arc::new(SocketHandler {
        task: fixture_task(),
        get_calls: AtomicUsize::new(0),
        last_cancel_preconditions: Mutex::new(None),
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test listener");
    let address = listener.local_addr().expect("read listener address");
    let incoming = TcpListenerStream::new(listener);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server_handler = Arc::clone(&handler);
    let server = tokio::spawn(async move {
        solti_api::tonic::transport::Server::builder()
            .add_service(
                GrpcApi::new(server_handler)
                    .with_auth(Token::new("socket-test-token").expect("valid test token"))
                    .server(),
            )
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let mut client = TaskServiceClient::connect(format!("http://{address}"))
        .await
        .expect("connect generated client");

    let unauthenticated = client
        .get_task(GetTaskRequest {
            name: "socket-task".into(),
        })
        .await
        .expect_err("missing bearer metadata must be rejected");
    assert_eq!(unauthenticated.code(), Code::Unauthenticated);
    assert_eq!(handler.get_calls.load(Ordering::SeqCst), 0);

    let response = client
        .get_task(authenticated(GetTaskRequest {
            name: "socket-task".into(),
        }))
        .await
        .expect("authenticated unary call succeeds")
        .into_inner();
    let task = response.task.expect("GetTask returns a task");
    let metadata = task.metadata.expect("task metadata is present");
    assert_eq!(metadata.name, "socket-task");
    assert_eq!(handler.get_calls.load(Ordering::SeqCst), 1);

    client
        .cancel_task(authenticated(CancelTaskRequest {
            name: "socket-task".into(),
            preconditions: Some(WireWritePreconditions {
                uid: Some(metadata.uid.clone()),
                resource_version: Some(metadata.resource_version.clone()),
            }),
        }))
        .await
        .expect("authenticated cancel succeeds");
    let cancel_preconditions = handler
        .last_cancel_preconditions
        .lock()
        .expect("cancel precondition lock is not poisoned")
        .clone()
        .expect("cancel reaches the handler");
    assert_eq!(
        cancel_preconditions.uid().map(ToString::to_string),
        Some(metadata.uid)
    );
    assert_eq!(
        cancel_preconditions.resource_version(),
        Some(metadata.resource_version.as_str())
    );

    let mut watch = client
        .watch_tasks(authenticated(WatchTasksRequest::default()))
        .await
        .expect("watch starts")
        .into_inner();
    let added = watch
        .message()
        .await
        .expect("first watch frame decodes")
        .expect("watch emits an added event");
    assert_eq!(
        TaskWatchEventType::try_from(added.r#type).expect("known watch event type"),
        TaskWatchEventType::Added
    );
    let expired = watch
        .message()
        .await
        .expect_err("terminal domain stream error reaches the client");
    assert_eq!(expired.code(), Code::OutOfRange);
    drop(watch);

    let oversized = client
        .get_task(authenticated(GetTaskRequest {
            name: "x".repeat(MAX_REQUEST_BYTES + 1),
        }))
        .await
        .expect_err("server decoding limit rejects an oversized message");
    assert_eq!(oversized.code(), Code::OutOfRange);
    assert_eq!(handler.get_calls.load(Ordering::SeqCst), 1);

    drop(client);
    shutdown_tx.send(()).expect("server is still running");
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("gRPC server shuts down within the bound")
        .expect("gRPC server task does not panic")
        .expect("gRPC server exits cleanly");
}
