//! # gRPC Transport
//!
//! Tonic service for protobuf package `solti.task.v1`.
//! Every RPC delegates to [`ApiHandler`].
//!
//! [`GrpcApi`] installs message limits, optional metrics, and access-control hooks.
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
//! | `ListTaskRuns`   | Unary            | Paged run history |
//! | `CancelTask`     | Unary            | Cancel      |
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

use solti_model::{LabelSelector, TaskFilter, TaskQuery, TaskRunQuery, Token, Uid};

#[cfg(test)]
use crate::auth::StaticBearerAuthenticator;
use crate::auth::bearer_value;
use crate::handler::ApiHandler;
use crate::metrics::{
    ApiMetricsHandle, InFlightGuard, RequestMetrics, Transport, noop_api_metrics,
    panic_contained_api_metrics, record_request,
};
use crate::proto_api::{
    self, task_service_server::TaskService, task_service_server::TaskServiceServer,
};
use crate::validate::{parse_list_limit, parse_task_id, parse_task_run_limit, validate_slot};
use crate::{
    ApiAuthenticatorHandle, ApiAuthorizerHandle, ApiIdentity, AuthenticationRequest,
    AuthorizationRequest, GRPC_API_SERVICE, TaskOperation, TaskTarget,
};

mod convert;
use convert::{
    output_event_to_proto, proto_to_domain_phase, runs_page_to_proto_bounded,
    task_watch_event_to_proto, tasks_page_to_proto_bounded, write_preconditions_from_proto,
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
/// Use [`GrpcApi`] to also install message limits and access control.
///
/// ## See Also
///
/// - [`GrpcApi`] builds the configured public service.
/// - [`ApiError`](crate::ApiError) defines gRPC error mapping.
pub struct TaskApiService<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
    authenticator: Option<ApiAuthenticatorHandle>,
    authorizer: Option<ApiAuthorizerHandle>,
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
    ///
    /// The installed backend is sticky-disabled after its first observed panic.
    pub fn new_with_metrics(handler: Arc<H>, metrics: ApiMetricsHandle) -> Self {
        Self::new_with_access(handler, panic_contained_api_metrics(metrics), None, None)
    }

    fn new_with_access(
        handler: Arc<H>,
        metrics: ApiMetricsHandle,
        authenticator: Option<ApiAuthenticatorHandle>,
        authorizer: Option<ApiAuthorizerHandle>,
    ) -> Self {
        Self {
            handler,
            metrics,
            authenticator,
            authorizer,
        }
    }

    async fn authenticate<T>(&self, request: &Request<T>) -> Result<Option<ApiIdentity>, Status> {
        let Some(authenticator) = &self.authenticator else {
            return Ok(request.extensions().get::<ApiIdentity>().cloned());
        };
        let credential = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_value);
        authenticator
            .authenticate(AuthenticationRequest::new(Transport::Grpc, credential))
            .await
            .map(Some)
            .map_err(Status::from)
    }

    async fn authorize(
        &self,
        identity: Option<&ApiIdentity>,
        operation: TaskOperation,
        target: TaskTarget<'_>,
    ) -> Result<(), Status> {
        let Some(authorizer) = &self.authorizer else {
            return Ok(());
        };
        authorizer
            .authorize(AuthorizationRequest::new(identity, operation, target))
            .await
            .map_err(Status::from)
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
/// The [`BearerAuth`] interceptor remains present for the built-in static token.
/// Custom authenticators run inside [`TaskApiService`].
pub type GrpcServer<H> = InterceptedService<TaskServiceServer<TaskApiService<H>>, BearerAuth>;

/// Builder for the tonic task API.
///
/// Authentication, authorization, and metrics are optional.
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
    authenticator: Option<ApiAuthenticatorHandle>,
    authorizer: Option<ApiAuthorizerHandle>,
}

impl<H> GrpcApi<H>
where
    H: ApiHandler,
{
    /// Creates a gRPC API for one handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self {
            handler,
            metrics: panic_contained_api_metrics(noop_api_metrics()),
            auth: None,
            authenticator: None,
            authorizer: None,
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
        self.authenticator = None;
        self
    }

    /// Installs an application-owned bearer authenticator.
    ///
    /// It returns the [`ApiIdentity`] used by the optional authorizer.
    pub fn with_authenticator(mut self, authenticator: ApiAuthenticatorHandle) -> Self {
        self.auth = None;
        self.authenticator = Some(authenticator);
        self
    }

    /// Installs an application-owned Task API authorization policy.
    ///
    /// The policy runs after wire validation and before the handler operation.
    pub fn with_authorizer(mut self, authorizer: ApiAuthorizerHandle) -> Self {
        self.authorizer = Some(authorizer);
        self
    }

    /// Attaches a metrics backend.
    ///
    /// The default backend ignores every update.
    /// The installed backend is sticky-disabled after its first observed panic.
    pub fn with_metrics(mut self, metrics: ApiMetricsHandle) -> Self {
        self.metrics = panic_contained_api_metrics(metrics);
        self
    }

    /// Builds the configured gRPC service.
    ///
    /// Encoded and decoded messages are limited to [`MAX_REQUEST_BYTES`](crate::MAX_REQUEST_BYTES).
    /// Task and TaskRun list pages are shaped before encoding to the same byte ceiling.
    pub fn server(self) -> GrpcServer<H> {
        let inner = TaskServiceServer::new(TaskApiService::new_with_access(
            self.handler,
            Arc::clone(&self.metrics),
            self.authenticator,
            self.authorizer,
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

/// Bearer interceptor used by [`GrpcServer`] for [`GrpcApi::with_auth`].
///
/// A valid static token installs an authenticated identity without an individual subject.
/// Without a configured token, every call passes through.
#[derive(Clone)]
pub struct BearerAuth {
    expected: Option<Token>,
    metrics: ApiMetricsHandle,
}

impl Interceptor for BearerAuth {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let Some(expected) = &self.expected else {
            return Ok(request);
        };
        let valid = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_value)
            .map(|presented| expected.verify(presented))
            .unwrap_or(false);

        if valid {
            request
                .extensions_mut()
                .insert(ApiIdentity::authenticated());
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
    record_request(
        metrics,
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
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let manifest = req
                .manifest
                .ok_or_else(|| Status::invalid_argument("missing manifest"))?;
            let manifest = convert::task_manifest_from_proto(manifest).map_err(Status::from)?;
            self.authorize(
                identity.as_ref(),
                TaskOperation::Create,
                TaskTarget::Manifest(&manifest),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "create",
                task_name = %manifest.name(),
                "task operation started"
            );
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
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let manifest = req
                .manifest
                .ok_or_else(|| Status::invalid_argument("missing manifest"))?;
            let manifest = convert::task_manifest_from_proto(manifest).map_err(Status::from)?;
            let preconditions =
                write_preconditions_from_proto(req.preconditions).map_err(Status::from)?;
            self.authorize(
                identity.as_ref(),
                TaskOperation::Apply,
                TaskTarget::Manifest(&manifest),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "apply",
                task_name = %manifest.name(),
                "task operation started"
            );
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
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            self.authorize(
                identity.as_ref(),
                TaskOperation::Get,
                TaskTarget::Task(&task_id),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "get",
                task_name = %task_id,
                "task operation started"
            );

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
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let filter = task_filter_from_wire(req.slot, req.phases, req.label_selector)
                .map_err(Status::from)?;
            let mut query = TaskQuery::from_filter(filter);

            query = query
                .with_limit(parse_list_limit(req.limit).map_err(Status::from)?)
                .with_item_byte_limit(
                    std::num::NonZeroUsize::new(crate::MAX_TASK_LIST_RESPONSE_BYTES)
                        .expect("the Task list response limit is positive"),
                );
            if !req.r#continue.is_empty() {
                let continuation =
                    crate::continuation::decode(&req.r#continue).map_err(Status::from)?;
                crate::continuation::validate_continuation_filter(&continuation, query.filter())
                    .map_err(Status::from)?;
                query = query.with_continuation(continuation);
            }

            let validation_query = query.clone();

            self.authorize(
                identity.as_ref(),
                TaskOperation::List,
                TaskTarget::Collection,
            )
            .await?;

            let page = self
                .handler
                .query_tasks(query)
                .await
                .map_err(Status::from)?;
            crate::continuation::validate_page(&page, &validation_query).map_err(Status::from)?;

            let response = tasks_page_to_proto_bounded(page, validation_query.filter())
                .map_err(Status::from)?;
            debug!(
                event = "api.operation_completed",
                transport = "grpc",
                operation = "list",
                count = response.tasks.len(),
                remaining = response.remaining_item_count.unwrap_or_default(),
                "task operation completed"
            );
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
            let identity = self.authenticate(&request).await?;
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

            self.authorize(
                identity.as_ref(),
                TaskOperation::Watch,
                TaskTarget::Collection,
            )
            .await?;

            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "watch",
                "task operation started"
            );

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
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            let mut query = TaskRunQuery::new()
                .with_limit(parse_task_run_limit(req.limit).map_err(Status::from)?)
                .with_item_byte_limit(
                    std::num::NonZeroUsize::new(crate::MAX_TASK_RUN_LIST_RESPONSE_BYTES)
                        .expect("the TaskRun list response limit is positive"),
                );
            if !req.r#continue.is_empty() {
                let continuation =
                    crate::continuation::decode_run(&req.r#continue).map_err(Status::from)?;
                crate::continuation::validate_run_continuation_task(&continuation, &task_id)
                    .map_err(Status::from)?;
                query = query.with_continuation(continuation);
            }
            let validation_query = query.clone();
            self.authorize(
                identity.as_ref(),
                TaskOperation::ListRuns,
                TaskTarget::Task(&task_id),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "list_runs",
                task_name = %task_id,
                "task operation started"
            );

            let page = self
                .handler
                .query_task_runs(&task_id, query)
                .await
                .map_err(Status::from)?;
            crate::continuation::validate_run_page(&page, &task_id, &validation_query)
                .map_err(Status::from)?;
            let response = runs_page_to_proto_bounded(page).map_err(Status::from)?;
            debug!(
                event = "api.operation_completed",
                transport = "grpc",
                operation = "list_runs",
                task_name = %task_id,
                count = response.runs.len(),
                remaining = response.remaining_item_count.unwrap_or_default(),
                "task operation completed"
            );

            Ok(Response::new(response))
        })
        .await
    }

    async fn delete_task(
        &self,
        request: Request<proto_api::DeleteTaskRequest>,
    ) -> Result<Response<proto_api::DeleteTaskResponse>, Status> {
        self.instrument("DeleteTask", async move {
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            let preconditions =
                write_preconditions_from_proto(req.preconditions).map_err(Status::from)?;
            self.authorize(
                identity.as_ref(),
                TaskOperation::Delete,
                TaskTarget::Task(&task_id),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "delete",
                task_name = %task_id,
                "task operation started"
            );

            self.handler
                .delete_task(&task_id, preconditions)
                .await
                .map_err(Status::from)?;

            Ok(Response::new(proto_api::DeleteTaskResponse {}))
        })
        .await
    }

    async fn cancel_task(
        &self,
        request: Request<proto_api::CancelTaskRequest>,
    ) -> Result<Response<proto_api::CancelTaskResponse>, Status> {
        self.instrument("CancelTask", async move {
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();

            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            let preconditions =
                write_preconditions_from_proto(req.preconditions).map_err(Status::from)?;
            self.authorize(
                identity.as_ref(),
                TaskOperation::Cancel,
                TaskTarget::Task(&task_id),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "cancel",
                task_name = %task_id,
                "task operation started"
            );

            self.handler
                .cancel_task(&task_id, preconditions)
                .await
                .map_err(Status::from)?;

            Ok(Response::new(proto_api::CancelTaskResponse {}))
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
            let identity = self.authenticate(&request).await?;
            let req = request.into_inner();
            let task_id = parse_task_id("task name", req.name).map_err(Status::from)?;
            let task_uid = Uid::new(req.task_uid)
                .map_err(|error| Status::invalid_argument(format!("invalid task_uid: {error}")))?;
            self.authorize(
                identity.as_ref(),
                TaskOperation::StreamLogs,
                TaskTarget::Task(&task_id),
            )
            .await?;
            debug!(
                event = "api.operation",
                transport = "grpc",
                operation = "stream_logs",
                task_name = %task_id,
                task_uid = %task_uid,
                "task operation started"
            );

            let domain_stream = self
                .handler
                .stream_task_logs(&task_id, &task_uid)
                .await
                .map_err(Status::from)?;

            let proto_stream = domain_stream
                .map(move |event| output_event_to_proto(event, &task_uid).map_err(Status::from));
            let stream: Self::StreamTaskLogsStream = Box::pin(proto_stream);
            Ok(stream)
        })
        .await
    }
}

#[cfg(test)]
mod tests;
