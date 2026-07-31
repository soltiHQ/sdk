//! # gRPC Transport
//!
//! Tonic service for protobuf package `solti.task.v1`.
//! Every RPC delegates to [`ApiHandler`].
//!
//! [`GrpcApi`] installs message limits, optional metrics, and bearer authentication.
//! [`wire`] exposes the generated client, server, and message types.
//!
//! ## RPCs
//!
//! | RPC              | Shape            | Operation   |
//! |------------------|------------------|-------------|
//! | `CreateTask`     | Unary            | Create      |
//! | `ApplyTask`      | Unary            | Apply       |
//! | `GetTask`        | Unary            | Get         |
//! | `ListTasks`      | Unary            | List        |
//! | `WatchTasks`     | Server streaming | Watch       |
//! | `ListTaskRuns`   | Unary            | Run history |
//! | `DeleteTask`     | Unary            | Delete      |
//! | `StreamTaskLogs` | Server streaming | Live output |
//!
//! Domain failures become `tonic::Status`.
//! Stream failures terminate the corresponding stream.

use std::pin::Pin;
use std::sync::Arc;

use tokio_stream::{Stream, StreamExt};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Response, Status};
use tracing::debug;

use solti_model::{LabelSelector, TaskFilter, TaskQuery, Token};

use crate::GRPC_API_SERVICE;
use crate::auth::bearer_value;
use crate::handler::ApiHandler;
use crate::metrics::{
    ApiMetricsHandle, InFlightGuard, RequestMetrics, Transport, noop_api_metrics,
};
use crate::proto_api::{
    self, task_service_server::TaskService, task_service_server::TaskServiceServer,
};
use crate::validate::{parse_list_limit, parse_task_id, validate_slot};

mod convert;
use convert::{
    output_event_to_proto, proto_to_domain_phase, task_watch_event_to_proto, tasks_page_to_proto,
    write_preconditions_from_proto,
};

/// Generated protobuf types for the current Task API.
///
/// This module includes messages, enums, [`crate::grpc::wire::TaskServiceClient`], and [`crate::grpc::wire::TaskServiceServer`].
///
/// ## Example
///
/// ```rust,no_run
/// use solti_api::grpc::wire::TaskServiceClient;
///
/// async fn connect() -> Result<(), solti_api::tonic::transport::Error> {
///     let client = TaskServiceClient::connect("http://127.0.0.1:50052").await?;
///     let _ = client;
///     Ok(())
/// }
/// ```
pub mod wire {
    pub use crate::proto_api::task_service_client::TaskServiceClient;
    pub use crate::proto_api::task_service_server::TaskServiceServer;
    pub use crate::proto_api::*;
}

type ServerStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

struct GrpcMetricsStream<T> {
    inner: ServerStream<T>,
    request: RequestMetrics,
}

impl<T> Stream for GrpcMetricsStream<T> {
    type Item = Result<T, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            std::task::Poll::Ready(Some(Err(status))) => {
                self.request.complete(status.code() as u16);
                std::task::Poll::Ready(Some(Err(status)))
            }
            std::task::Poll::Ready(None) => {
                self.request.complete(tonic::Code::Ok as u16);
                std::task::Poll::Ready(None)
            }
            poll => poll,
        }
    }
}

/// Generated `TaskService` implementation over an [`ApiHandler`].
///
/// This is the lower-level service implementation.
/// It converts protobuf values and records optional metrics.
/// Use [`GrpcApi`] to also install message limits and authentication.
///
/// ## See Also
///
/// - [`GrpcApi`] builds the configured public service.
/// - [`ApiError`](crate::ApiError) defines gRPC error mapping.
pub struct TaskApiService<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
}

impl<H> TaskApiService<H>
where
    H: ApiHandler,
{
    /// Creates a service with the no-op metrics backend.
    pub fn new(handler: Arc<H>) -> Self {
        Self::new_with_metrics(handler, noop_api_metrics())
    }

    /// Creates a service with an explicit metrics backend.
    pub fn new_with_metrics(handler: Arc<H>, metrics: ApiMetricsHandle) -> Self {
        Self { handler, metrics }
    }

    async fn instrument<F, T>(&self, method: &'static str, fut: F) -> Result<Response<T>, Status>
    where
        F: Future<Output = Result<Response<T>, Status>>,
    {
        let path = format!("/{GRPC_API_SERVICE}/{method}");
        let mut request = RequestMetrics::enter(&self.metrics, Transport::Grpc, method, path);
        let result = fut.await;
        let status = match &result {
            Ok(_) => 0u16,
            Err(s) => s.code() as u16,
        };
        request.complete(status);
        result
    }

    async fn instrument_stream<F, T>(
        &self,
        method: &'static str,
        fut: F,
    ) -> Result<Response<ServerStream<T>>, Status>
    where
        F: Future<Output = Result<ServerStream<T>, Status>>,
        T: 'static,
    {
        let path = format!("/{GRPC_API_SERVICE}/{method}");
        let mut request = RequestMetrics::enter(&self.metrics, Transport::Grpc, method, path);
        match fut.await {
            Ok(stream) => {
                let stream: ServerStream<T> = Box::pin(GrpcMetricsStream {
                    inner: stream,
                    request,
                });
                Ok(Response::new(stream))
            }
            Err(status) => {
                request.complete(status.code() as u16);
                Err(status)
            }
        }
    }
}

/// Complete gRPC service returned by [`GrpcApi::server`].
///
/// The [`BearerAuth`] interceptor is always present.
/// It passes calls through when no token is configured.
pub type GrpcServer<H> = InterceptedService<TaskServiceServer<TaskApiService<H>>, BearerAuth>;

/// Builder for the tonic task API.
///
/// Authentication and metrics are optional.
/// [`server`](Self::server) installs the public message-size limit.
///
/// ## Example
///
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use solti_api::{ApiHandler, GrpcApi};
/// # async fn example<H: ApiHandler>(adapter: Arc<H>) -> Result<(), Box<dyn std::error::Error>> {
/// let svc = GrpcApi::new(adapter).server();
/// solti_api::tonic::transport::Server::builder()
///     .add_service(svc)
///     .serve("0.0.0.0:50052".parse()?)
///     .await?;
/// # Ok(()) }
/// ```
///
/// ## See Also
///
/// - [`ApiHandler`] defines the backend operations.
/// - [`ApiError`](crate::ApiError) defines the gRPC status mapping.
pub struct GrpcApi<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
    auth: Option<Token>,
}

impl<H> GrpcApi<H>
where
    H: ApiHandler,
{
    /// Creates a gRPC API for one handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self {
            handler,
            metrics: noop_api_metrics(),
            auth: None,
        }
    }

    /// Requires a bearer token on every call.
    ///
    /// The expected metadata is `authorization: Bearer <token>`.
    /// Missing or invalid credentials return `Unauthenticated`.
    /// Rejected calls do not reach the handler.
    /// Authentication is disabled when this method is not called.
    pub fn with_auth(mut self, token: Token) -> Self {
        self.auth = Some(token);
        self
    }

    /// Attaches a metrics backend.
    ///
    /// The default backend ignores every update.
    pub fn with_metrics(mut self, metrics: ApiMetricsHandle) -> Self {
        self.metrics = metrics;
        self
    }

    /// Builds the configured gRPC service.
    ///
    /// Encoded and decoded messages are limited to
    /// [`MAX_REQUEST_BYTES`](crate::MAX_REQUEST_BYTES).
    /// The returned service always contains [`BearerAuth`].
    pub fn server(self) -> GrpcServer<H> {
        let inner = TaskServiceServer::new(TaskApiService::new_with_metrics(
            self.handler,
            Arc::clone(&self.metrics),
        ))
        .max_decoding_message_size(crate::MAX_REQUEST_BYTES)
        .max_encoding_message_size(crate::MAX_REQUEST_BYTES);
        InterceptedService::new(
            inner,
            BearerAuth {
                expected: self.auth,
                metrics: self.metrics,
            },
        )
    }
}

/// Bearer interceptor used by [`GrpcServer`].
///
/// It verifies `authorization: Bearer <token>` metadata.
/// Token comparison uses [`Token::verify`].
/// Without a configured token, every call passes through.
/// Configure it through [`GrpcApi::with_auth`].
#[derive(Clone)]
pub struct BearerAuth {
    expected: Option<Token>,
    metrics: ApiMetricsHandle,
}

impl Interceptor for BearerAuth {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.expected else {
            return Ok(request);
        };
        let ok = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_value)
            .map(|presented| expected.verify(presented))
            .unwrap_or(false);

        if ok {
            Ok(request)
        } else {
            record_auth_failure(&self.metrics, &request);
            Err(Status::unauthenticated("missing or invalid bearer token"))
        }
    }
}

fn record_auth_failure(metrics: &ApiMetricsHandle, request: &Request<()>) {
    let method = request
        .extensions()
        .get::<tonic::GrpcMethod<'static>>()
        .map(tonic::GrpcMethod::method)
        .unwrap_or("<unknown>");
    let path = request
        .extensions()
        .get::<tonic::GrpcMethod<'static>>()
        .map(|grpc| format!("/{}/{}", grpc.service(), grpc.method()))
        .unwrap_or_else(|| "<unknown>".to_owned());
    let _in_flight = InFlightGuard::enter(metrics, Transport::Grpc);
    metrics.record_request(
        Transport::Grpc,
        method,
        &path,
        tonic::Code::Unauthenticated as u16,
        0,
    );
}

fn task_filter_from_wire(
    slot: Option<String>,
    phases: Vec<i32>,
    label_selector: String,
) -> Result<TaskFilter, crate::ApiError> {
    let mut filter = TaskFilter::new();

    if let Some(slot) = slot {
        filter = filter.with_slot(validate_slot(slot)?);
    }

    for phase_raw in phases {
        filter = filter.with_phase(proto_to_domain_phase(phase_raw)?);
    }

    if !label_selector.is_empty() {
        let selector = label_selector.parse::<LabelSelector>().map_err(|error| {
            crate::ApiError::InvalidRequest(format!("invalid labelSelector: {error}"))
        })?;
        filter = filter
            .with_label_selector(selector)
            .map_err(|error| crate::ApiError::InvalidRequest(error.to_string()))?;
    }

    Ok(filter)
}

#[tonic::async_trait]
impl<H> TaskService for TaskApiService<H>
where
    H: ApiHandler,
{
    async fn create_task(
        &self,
        request: Request<proto_api::CreateTaskRequest>,
    ) -> Result<Response<proto_api::CreateTaskResponse>, Status> {
        self.instrument("CreateTask", async move {
            let req = request.into_inner();

            let manifest = req
                .manifest
                .ok_or_else(|| Status::invalid_argument("missing manifest"))?;
            let manifest = convert::task_manifest_from_proto(manifest).map_err(Status::from)?;
            debug!(name = %manifest.name(), "grpc: creating task");
            let task = self
                .handler
                .create_task(manifest)
                .await
                .map_err(Status::from)?;
            let task = proto_api::Task::try_from(task).map_err(Status::from)?;

            Ok(Response::new(proto_api::CreateTaskResponse {
                task: Some(task),
            }))
        })
        .await
    }

    async fn apply_task(
        &self,
        request: Request<proto_api::ApplyTaskRequest>,
    ) -> Result<Response<proto_api::ApplyTaskResponse>, Status> {
        self.instrument("ApplyTask", async move {
            let req = request.into_inner();

            let manifest = req
                .manifest
                .ok_or_else(|| Status::invalid_argument("missing manifest"))?;
            let manifest = convert::task_manifest_from_proto(manifest).map_err(Status::from)?;
            let preconditions =
                write_preconditions_from_proto(req.preconditions).map_err(Status::from)?;
            debug!(name = %manifest.name(), "grpc: applying task");
            let task = self
                .handler
                .apply_task(manifest, preconditions)
                .await
                .map_err(Status::from)?;
            let task = proto_api::Task::try_from(task).map_err(Status::from)?;

            Ok(Response::new(proto_api::ApplyTaskResponse {
                task: Some(task),
            }))
        })
        .await
    }

    async fn get_task(
        &self,
        request: Request<proto_api::GetTaskRequest>,
    ) -> Result<Response<proto_api::GetTaskResponse>, Status> {
        self.instrument("GetTask", async move {
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            debug!(%task_id, "grpc: getting task status");

            let task = self
                .handler
                .get_task(&task_id)
                .await
                .map_err(Status::from)?
                .ok_or_else(|| Status::from(crate::ApiError::TaskNotFound(task_id.to_string())))?;

            let task = proto_api::Task::try_from(task).map_err(Status::from)?;

            Ok(Response::new(proto_api::GetTaskResponse {
                task: Some(task),
            }))
        })
        .await
    }

    async fn list_tasks(
        &self,
        request: Request<proto_api::ListTasksRequest>,
    ) -> Result<Response<proto_api::ListTasksResponse>, Status> {
        self.instrument("ListTasks", async move {
            let req = request.into_inner();

            let filter = task_filter_from_wire(req.slot, req.phases, req.label_selector)
                .map_err(Status::from)?;
            let mut query = TaskQuery::from_filter(filter);

            query = query.with_limit(parse_list_limit(req.limit).map_err(Status::from)?);
            if !req.r#continue.is_empty() {
                query = query.with_continuation(
                    crate::continuation::decode(&req.r#continue).map_err(Status::from)?,
                );
            }

            let page_filter = query.filter().clone();
            let page_limit = query.limit();
            let page = self
                .handler
                .query_tasks(query)
                .await
                .map_err(Status::from)?;
            crate::continuation::validate_page(&page, &page_filter, page_limit)
                .map_err(Status::from)?;

            debug!(
                count = page.items.len(),
                remaining = page.remaining_item_count,
                "grpc: tasks listed"
            );

            let response = tasks_page_to_proto(page).map_err(Status::from)?;
            Ok(Response::new(response))
        })
        .await
    }

    /// Server-streaming Task collection watch.
    type WatchTasksStream = ServerStream<proto_api::WatchTasksResponse>;

    async fn watch_tasks(
        &self,
        request: Request<proto_api::WatchTasksRequest>,
    ) -> Result<Response<Self::WatchTasksStream>, Status> {
        self.instrument_stream("WatchTasks", async move {
            let req = request.into_inner();
            let filter = task_filter_from_wire(req.slot, req.phases, req.label_selector)
                .map_err(Status::from)?;
            if req
                .resource_version
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(Status::invalid_argument(
                    "resource_version must not be empty",
                ));
            }

            let domain_stream = self
                .handler
                .watch_tasks(filter, req.resource_version)
                .await
                .map_err(Status::from)?;
            let proto_stream = domain_stream.map(|event| match event {
                Ok(event) => task_watch_event_to_proto(event).map_err(Status::from),
                Err(error) => Err(Status::from(error)),
            });
            let stream: Self::WatchTasksStream = Box::pin(proto_stream);
            Ok(stream)
        })
        .await
    }

    async fn list_task_runs(
        &self,
        request: Request<proto_api::ListTaskRunsRequest>,
    ) -> Result<Response<proto_api::ListTaskRunsResponse>, Status> {
        self.instrument("ListTaskRuns", async move {
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            debug!(%task_id, "grpc: listing task runs");

            let runs = self
                .handler
                .list_task_runs(&task_id)
                .await
                .map_err(Status::from)?;

            let runs = runs
                .into_iter()
                .map(proto_api::TaskRunInfo::try_from)
                .collect::<Result<_, _>>()
                .map_err(Status::from)?;

            Ok(Response::new(proto_api::ListTaskRunsResponse { runs }))
        })
        .await
    }

    async fn delete_task(
        &self,
        request: Request<proto_api::DeleteTaskRequest>,
    ) -> Result<Response<proto_api::DeleteTaskResponse>, Status> {
        self.instrument("DeleteTask", async move {
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            let preconditions =
                write_preconditions_from_proto(req.preconditions).map_err(Status::from)?;
            debug!(%task_id, "grpc: deleting task");

            self.handler
                .delete_task(&task_id, preconditions)
                .await
                .map_err(Status::from)?;

            debug!(%task_id, "grpc: task deleted");
            Ok(Response::new(proto_api::DeleteTaskResponse {}))
        })
        .await
    }

    /// Server-streaming RPC.
    type StreamTaskLogsStream = ServerStream<proto_api::StreamTaskLogsResponse>;

    async fn stream_task_logs(
        &self,
        request: Request<proto_api::StreamTaskLogsRequest>,
    ) -> Result<Response<Self::StreamTaskLogsStream>, Status> {
        self.instrument_stream("StreamTaskLogs", async move {
            let req = request.into_inner();
            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            debug!(%task_id, "grpc: subscribing to task log stream");

            let domain_stream = self
                .handler
                .stream_task_logs(&task_id)
                .await
                .map_err(Status::from)?;

            let proto_stream =
                domain_stream.map(|event| output_event_to_proto(event).map_err(Status::from));
            let stream: Self::StreamTaskLogsStream = Box::pin(proto_stream);
            Ok(stream)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::{Duration, UNIX_EPOCH};

    use async_trait::async_trait;
    use bytes::Bytes;
    use solti_model::{
        ExtensionWorkload, OutputChunk, OutputEvent, StreamKind as ModelStreamKind, Task,
        TaskContinuation, TaskFilter, TaskId, TaskManifest, TaskPage, TaskPhase, TaskQuery,
        TaskRun, TaskSpec, TaskWatchEvent, TaskWorkload, WORKLOAD_API_VERSION, WorkloadTypeMeta,
        WritePreconditions,
    };

    use crate::error::ApiError;
    use crate::handler::{ApiHandler, OutputEventStream, TaskWatchEventStream};

    #[derive(Default)]
    struct StreamMock {
        last_preconditions: std::sync::Mutex<Option<WritePreconditions>>,
        last_query: std::sync::Mutex<Option<TaskQuery>>,
        last_watch_filter: std::sync::Mutex<Option<TaskFilter>>,
        last_watch_resource_version: std::sync::Mutex<Option<Option<String>>>,
        watch_expired: bool,
        watch_stream_expired: bool,
        log_stream_pending: bool,
    }

    #[async_trait]
    impl ApiHandler for StreamMock {
        async fn create_task(&self, _manifest: TaskManifest) -> Result<Task, ApiError> {
            unreachable!()
        }
        async fn apply_task(
            &self,
            manifest: TaskManifest,
            preconditions: WritePreconditions,
        ) -> Result<Task, ApiError> {
            *self.last_preconditions.lock().unwrap() = Some(preconditions);
            Task::from_manifest(manifest).map_err(|error| ApiError::Internal(error.to_string()))
        }
        async fn get_task(&self, _id: &TaskId) -> Result<Option<Task>, ApiError> {
            Ok(None)
        }
        async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
            *self.last_query.lock().unwrap() = Some(query);
            Ok(TaskPage {
                items: vec![],
                resource_version: "test:1".into(),
                continuation: None,
                remaining_item_count: 0,
            })
        }
        async fn watch_tasks(
            &self,
            filter: TaskFilter,
            resource_version: Option<String>,
        ) -> Result<TaskWatchEventStream, ApiError> {
            *self.last_watch_filter.lock().unwrap() = Some(filter);
            *self.last_watch_resource_version.lock().unwrap() = Some(resource_version);
            if self.watch_expired {
                return Err(ApiError::ResourceVersionExpired(
                    "requested resourceVersion is no longer retained".into(),
                ));
            }
            let mut events = vec![Ok(TaskWatchEvent::Added(watch_task()))];
            if self.watch_stream_expired {
                events.push(Err(ApiError::ResourceVersionExpired(
                    "watch position is no longer retained".into(),
                )));
            }
            Ok(Box::pin(tokio_stream::iter(events)))
        }
        async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError> {
            let workload = if id.as_str() == "embedded-run" {
                WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Embedded").unwrap()
            } else {
                WorkloadTypeMeta::new("workloads.example.io/v1", "DatabaseBackup").unwrap()
            };
            Ok(vec![TaskRun::starting(2, 1, workload).unwrap()])
        }
        async fn delete_task(
            &self,
            _id: &TaskId,
            preconditions: WritePreconditions,
        ) -> Result<(), ApiError> {
            *self.last_preconditions.lock().unwrap() = Some(preconditions);
            Ok(())
        }
        async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError> {
            if id.as_str() == "missing" {
                return Err(ApiError::TaskNotFound(id.to_string()));
            }
            if self.log_stream_pending {
                return Ok(Box::pin(tokio_stream::pending()));
            }
            let events = vec![
                OutputEvent::RunStarted {
                    generation: 2,
                    attempt: 1,
                    started_at: UNIX_EPOCH + Duration::from_millis(1000),
                },
                OutputEvent::Chunk(OutputChunk {
                    generation: 2,
                    attempt: 1,
                    stream: ModelStreamKind::Stdout,
                    seq: 0,
                    ts: UNIX_EPOCH + Duration::from_millis(1100),
                    line: Bytes::from_static(b"hello-grpc"),
                }),
                OutputEvent::RunFinished {
                    generation: 2,
                    attempt: 1,
                    exit_code: Some(0),
                    finished_at: UNIX_EPOCH + Duration::from_millis(1500),
                },
            ];
            Ok(Box::pin(tokio_stream::iter(events)))
        }
    }

    fn service() -> TaskApiService<StreamMock> {
        TaskApiService::new(Arc::new(StreamMock::default()))
    }

    fn watch_task() -> Task {
        let workload = TaskWorkload::Extension(
            ExtensionWorkload::new(
                "workloads.example.io/v1",
                "ExampleJob",
                serde_json::json!({"value": 1}),
            )
            .unwrap(),
        );
        let spec = TaskSpec::builder("primary", workload, 5_000_u64)
            .build()
            .unwrap();
        let mut task = Task::new("watch-task", spec).unwrap();
        task.set_resource_version("test:2").unwrap();
        task
    }

    #[tokio::test]
    async fn get_task_maps_missing_resource_to_not_found_status() {
        let status = service()
            .get_task(Request::new(proto_api::GetTaskRequest {
                name: "missing".into(),
            }))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn delete_task_forwards_write_preconditions() {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));

        service
            .delete_task(Request::new(proto_api::DeleteTaskRequest {
                name: "task-1".into(),
                preconditions: Some(proto_api::WritePreconditions {
                    uid: Some("uid-1".into()),
                    resource_version: Some("17".into()),
                }),
            }))
            .await
            .unwrap();

        let preconditions = handler
            .last_preconditions
            .lock()
            .unwrap()
            .clone()
            .expect("handler received preconditions");
        assert_eq!(preconditions.uid().unwrap().as_str(), "uid-1");
        assert_eq!(preconditions.resource_version(), Some("17"));
    }

    #[tokio::test]
    async fn delete_task_rejects_empty_write_precondition() {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));

        let status = service
            .delete_task(Request::new(proto_api::DeleteTaskRequest {
                name: "task-1".into(),
                preconditions: Some(proto_api::WritePreconditions {
                    uid: None,
                    resource_version: Some(String::new()),
                }),
            }))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(handler.last_preconditions.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn list_tasks_forwards_filters_and_continuation() {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));
        let phases = vec![
            proto_api::TaskPhase::Pending as i32,
            proto_api::TaskPhase::Running as i32,
            proto_api::TaskPhase::Pending as i32,
        ];
        let label_selector = "environment=production,tier in (frontend,backend)";
        let filter = task_filter_from_wire(
            Some("primary".into()),
            phases.clone(),
            label_selector.into(),
        )
        .unwrap();
        let continuation =
            TaskContinuation::new("test:7", filter.clone(), TaskId::new("task-20").unwrap())
                .unwrap();

        service
            .list_tasks(Request::new(proto_api::ListTasksRequest {
                slot: Some("primary".into()),
                phases,
                limit: 25,
                label_selector: label_selector.into(),
                r#continue: crate::continuation::encode(continuation.clone()).unwrap(),
            }))
            .await
            .unwrap();

        let query = handler
            .last_query
            .lock()
            .unwrap()
            .take()
            .expect("handler received query");
        assert_eq!(query.slot().unwrap().as_str(), "primary");
        assert_eq!(query.phases(), &[TaskPhase::Pending, TaskPhase::Running]);
        assert_eq!(query.limit(), 25);
        assert_eq!(query.continuation(), Some(&continuation));
        assert_eq!(query.filter(), &filter);
        assert!(query.matches_labels(&{
            let mut labels = solti_model::Labels::new();
            labels
                .insert("environment", "production")
                .insert("tier", "backend");
            labels
        }));
    }

    #[tokio::test]
    async fn list_tasks_rejects_invalid_phase_or_label_selector_before_handler() {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));

        let phase = service
            .list_tasks(Request::new(proto_api::ListTasksRequest {
                phases: vec![proto_api::TaskPhase::Unspecified as i32],
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(phase.code(), tonic::Code::InvalidArgument);

        let selector = service
            .list_tasks(Request::new(proto_api::ListTasksRequest {
                label_selector: "tier in (".into(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(selector.code(), tonic::Code::InvalidArgument);

        let continuation = service
            .list_tasks(Request::new(proto_api::ListTasksRequest {
                r#continue: "not-a-token".into(),
                ..Default::default()
            }))
            .await
            .unwrap_err();
        assert_eq!(continuation.code(), tonic::Code::InvalidArgument);
        assert!(handler.last_query.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn watch_tasks_forwards_filters_and_resource_version() {
        let handler = Arc::new(StreamMock::default());
        let service = TaskApiService::new(Arc::clone(&handler));

        let mut stream = service
            .watch_tasks(Request::new(proto_api::WatchTasksRequest {
                slot: Some("primary".into()),
                phases: vec![
                    proto_api::TaskPhase::Pending as i32,
                    proto_api::TaskPhase::Running as i32,
                ],
                label_selector: "environment=production".into(),
                resource_version: Some("test:1".into()),
            }))
            .await
            .unwrap()
            .into_inner();

        let event = stream.next().await.unwrap().unwrap();
        assert_eq!(event.r#type, proto_api::TaskWatchEventType::Added as i32);
        assert_eq!(event.object.unwrap().metadata.unwrap().name, "watch-task");
        assert!(stream.next().await.is_none());

        let filter = handler
            .last_watch_filter
            .lock()
            .unwrap()
            .take()
            .expect("handler received watch filter");
        assert_eq!(filter.slot().unwrap().as_str(), "primary");
        assert_eq!(filter.phases(), &[TaskPhase::Pending, TaskPhase::Running]);
        let mut labels = solti_model::Labels::new();
        labels.insert("environment", "production");
        assert!(filter.matches_labels(&labels));
        assert_eq!(
            handler.last_watch_resource_version.lock().unwrap().take(),
            Some(Some("test:1".into()))
        );
    }

    #[tokio::test]
    async fn watch_tasks_maps_initial_expiration_to_out_of_range() {
        use std::sync::atomic::Ordering;

        let (probe, service) = probed_service_with(StreamMock {
            watch_expired: true,
            ..StreamMock::default()
        });

        let status = service
            .watch_tasks(Request::new(proto_api::WatchTasksRequest {
                resource_version: Some("old:1".into()),
                ..Default::default()
            }))
            .await
            .err()
            .expect("expired watch must fail");

        assert_eq!(status.code(), tonic::Code::OutOfRange);
        assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(
            probe.last_status.load(Ordering::SeqCst),
            tonic::Code::OutOfRange as u16
        );
    }

    #[tokio::test]
    async fn watch_tasks_maps_stream_expiration_to_out_of_range() {
        use std::sync::atomic::Ordering;

        let (probe, service) = probed_service_with(StreamMock {
            watch_stream_expired: true,
            ..StreamMock::default()
        });
        let mut stream = service
            .watch_tasks(Request::new(proto_api::WatchTasksRequest::default()))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);
        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);
        let status = stream.next().await.unwrap().unwrap_err();
        assert_eq!(status.code(), tonic::Code::OutOfRange);
        assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(
            probe.last_status.load(Ordering::SeqCst),
            tonic::Code::OutOfRange as u16
        );
        assert!(stream.next().await.is_none());
        assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn watch_tasks_rejects_invalid_input_before_handler() {
        for request in [
            proto_api::WatchTasksRequest {
                resource_version: Some(String::new()),
                ..Default::default()
            },
            proto_api::WatchTasksRequest {
                phases: vec![proto_api::TaskPhase::Unspecified as i32],
                ..Default::default()
            },
            proto_api::WatchTasksRequest {
                label_selector: "tier in (".into(),
                ..Default::default()
            },
        ] {
            let handler = Arc::new(StreamMock::default());
            let service = TaskApiService::new(Arc::clone(&handler));
            let status = service
                .watch_tasks(Request::new(request))
                .await
                .err()
                .expect("invalid watch must fail");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(handler.last_watch_filter.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn list_task_runs_exposes_historical_workload_gvk() {
        let response = service()
            .list_task_runs(Request::new(proto_api::ListTaskRunsRequest {
                name: "extension-run".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(response.runs.len(), 1);
        assert_eq!(
            response.runs[0].workload_api_version,
            "workloads.example.io/v1"
        );
        assert_eq!(response.runs[0].workload_kind, "DatabaseBackup");
    }

    #[tokio::test]
    async fn list_task_runs_guards_embedded_history_from_custom_handler() {
        let status = service()
            .list_task_runs(Request::new(proto_api::ListTaskRunsRequest {
                name: "embedded-run".into(),
            }))
            .await
            .unwrap_err();

        assert_eq!(status.code(), tonic::Code::Internal);
    }

    #[tokio::test]
    async fn stream_task_logs_returns_three_proto_events_in_order() {
        let svc = service();
        let req = Request::new(proto_api::StreamTaskLogsRequest {
            name: "task-1".into(),
        });

        let response = svc.stream_task_logs(req).await.expect("stream Ok");
        let mut stream = response.into_inner();

        match stream.next().await.unwrap().unwrap().kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::RunStarted(r) => {
                assert_eq!(r.generation, 2);
                assert_eq!(r.attempt, 1);
                assert_eq!(r.started_at, 1000);
            }
            other => panic!("expected RunStarted, got {other:?}"),
        }

        match stream.next().await.unwrap().unwrap().kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::Chunk(c) => {
                assert_eq!(c.generation, 2);
                assert_eq!(c.attempt, 1);
                assert_eq!(c.stream, proto_api::OutputStreamKind::Stdout as i32);
                assert_eq!(c.seq, 0);
                assert_eq!(&c.line[..], b"hello-grpc");
            }
            other => panic!("expected Chunk, got {other:?}"),
        }

        match stream.next().await.unwrap().unwrap().kind.unwrap() {
            proto_api::stream_task_logs_response::Kind::RunFinished(r) => {
                assert_eq!(r.generation, 2);
                assert_eq!(r.attempt, 1);
                assert_eq!(r.exit_code, Some(0));
                assert_eq!(r.finished_at, 1500);
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
        assert!(stream.next().await.is_none(), "stream must terminate");
    }

    #[tokio::test]
    async fn stream_task_logs_rejects_every_invalid_model_name() {
        let svc = service();
        for invalid in ["  ", "a/b", "a b", ".", "bad$name"] {
            let req = Request::new(proto_api::StreamTaskLogsRequest {
                name: invalid.into(),
            });
            let status = match svc.stream_task_logs(req).await {
                Err(s) => s,
                Ok(_) => panic!("expected error status for {invalid:?}"),
            };
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
    }

    #[tokio::test]
    async fn stream_task_logs_maps_task_not_found_to_not_found_status() {
        let svc = service();
        let req = Request::new(proto_api::StreamTaskLogsRequest {
            name: "missing".into(),
        });
        let status = match svc.stream_task_logs(req).await {
            Err(s) => s,
            Ok(_) => panic!("expected error status"),
        };
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    fn auth_interceptor(secret: &str) -> BearerAuth {
        BearerAuth {
            expected: Some(Token::new(secret).unwrap()),
            metrics: noop_api_metrics(),
        }
    }

    fn request_with_authorization(value: &str) -> Request<()> {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert("authorization", value.parse().expect("ascii metadata"));
        req
    }

    #[test]
    fn bearer_auth_rejects_invalid_credentials() {
        let requests = [
            Request::new(()),
            request_with_authorization("Bearer not-the-secret"),
            request_with_authorization("sekret"),
            request_with_authorization("Basic sekret"),
        ];

        for request in requests {
            let status = auth_interceptor("sekret").call(request).unwrap_err();
            assert_eq!(status.code(), tonic::Code::Unauthenticated);
        }
    }

    #[test]
    fn bearer_auth_accepts_valid_token_scheme_case_insensitively() {
        for header in ["Bearer sekret", "bearer sekret", "BEARER sekret"] {
            let mut auth = auth_interceptor("sekret");
            assert!(
                auth.call(request_with_authorization(header)).is_ok(),
                "header {header:?} must pass"
            );
        }
    }

    #[test]
    fn bearer_auth_passes_through_when_no_token_configured() {
        let mut auth = BearerAuth {
            expected: None,
            metrics: noop_api_metrics(),
        };
        assert!(auth.call(Request::new(())).is_ok());
        assert!(
            auth.call(request_with_authorization("Bearer anything"))
                .is_ok()
        );
    }

    #[derive(Debug, Default)]
    struct GaugeProbe {
        in_flight: std::sync::atomic::AtomicI64,
        completed: std::sync::atomic::AtomicUsize,
        last_status: std::sync::atomic::AtomicU16,
    }

    impl crate::metrics::ApiMetricsBackend for GaugeProbe {
        fn record_request(
            &self,
            _transport: crate::metrics::Transport,
            _method: &str,
            _path: &str,
            status: u16,
            _duration_ms: u64,
        ) {
            self.last_status
                .store(status, std::sync::atomic::Ordering::SeqCst);
            self.completed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn record_in_flight_delta(&self, _transport: crate::metrics::Transport, delta: i64) {
            self.in_flight
                .fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn probed_service() -> (Arc<GaugeProbe>, TaskApiService<StreamMock>) {
        probed_service_with(StreamMock::default())
    }

    fn probed_service_with(handler: StreamMock) -> (Arc<GaugeProbe>, TaskApiService<StreamMock>) {
        let probe = Arc::new(GaugeProbe::default());
        let handle: ApiMetricsHandle = probe.clone();
        (
            probe,
            TaskApiService::new_with_metrics(Arc::new(handler), handle),
        )
    }

    #[test]
    fn rejected_auth_is_recorded_and_balances_gauge() {
        use std::sync::atomic::Ordering;

        let probe = Arc::new(GaugeProbe::default());
        let metrics: ApiMetricsHandle = probe.clone();
        let mut auth = BearerAuth {
            expected: Some(Token::new("secret").unwrap()),
            metrics,
        };
        let mut request = Request::new(());
        request
            .extensions_mut()
            .insert(tonic::GrpcMethod::new(GRPC_API_SERVICE, "GetTask"));

        let status = auth.call(request).unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn instrument_records_completed_request_and_balances_gauge() {
        use std::sync::atomic::Ordering;

        let (probe, svc) = probed_service();
        let result = svc
            .instrument("Probe", async { Ok(Response::new(())) })
            .await;

        assert!(result.is_ok());
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_subscription_is_instrumented() {
        use std::sync::atomic::Ordering;

        let (probe, service) = probed_service();
        let mut stream = service
            .stream_task_logs(Request::new(proto_api::StreamTaskLogsRequest {
                name: "task-a".into(),
            }))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);

        while let Some(event) = stream.next().await {
            event.unwrap();
        }

        assert_eq!(probe.completed.load(Ordering::SeqCst), 1);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(
            probe.last_status.load(Ordering::SeqCst),
            tonic::Code::Ok as u16
        );
    }

    #[tokio::test]
    async fn dropping_server_stream_releases_gauge_without_completion() {
        use std::sync::atomic::Ordering;

        let (probe, service) = probed_service_with(StreamMock {
            log_stream_pending: true,
            ..StreamMock::default()
        });
        let response = service
            .stream_task_logs(Request::new(proto_api::StreamTaskLogsRequest {
                name: "task-a".into(),
            }))
            .await
            .unwrap();

        assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 1);

        drop(response);

        assert_eq!(probe.completed.load(Ordering::SeqCst), 0);
        assert_eq!(probe.in_flight.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn in_flight_gauge_recovers_when_rpc_future_is_dropped() {
        use std::future::Future;
        use std::sync::atomic::Ordering;
        use std::task::{Context, Poll, Waker};

        let (probe, svc) = probed_service();

        let mut fut = Box::pin(svc.instrument(
            "Probe",
            std::future::pending::<Result<Response<()>, Status>>(),
        ));

        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(
            probe.in_flight.load(Ordering::SeqCst),
            1,
            "gauge must be armed after the first poll"
        );

        drop(fut);
        assert_eq!(
            probe.in_flight.load(Ordering::SeqCst),
            0,
            "dropping the future must release the in-flight slot"
        );
        assert_eq!(
            probe.completed.load(Ordering::SeqCst),
            0,
            "a cancelled RPC must not be recorded as completed"
        );
    }
}
