//! # HTTP Transport
//!
//! Axum router for the model-owned CRD JSON representation.
//! Every operation delegates to [`ApiHandler`].
//!
//! The current API root is `/apis/solti.io/v1`.
//!
//! ## Routes
//!
//! | Method   | Path                                          | Operation   |
//! |----------|-----------------------------------------------|-------------|
//! | `POST`   | `/apis/solti.io/v1/tasks`                     | Create      |
//! | `PUT`    | `/apis/solti.io/v1/tasks/{name}`              | Apply       |
//! | `GET`    | `/apis/solti.io/v1/tasks/{name}`              | Get         |
//! | `GET`    | `/apis/solti.io/v1/tasks`                     | List        |
//! | `GET`    | `/apis/solti.io/v1/tasks?watch=true`          | Watch       |
//! | `GET`    | `/apis/solti.io/v1/tasks/{name}/runs`         | Run history |
//! | `GET`    | `/apis/solti.io/v1/tasks/{name}/logs`         | Live output |
//! | `DELETE` | `/apis/solti.io/v1/tasks/{name}`              | Delete      |
//!
//! ## Wire Shapes
//!
//! Create and apply accept `TaskManifest` JSON.
//! Resource reads return `Task` JSON.
//! Lists return a Kubernetes-style `TaskList`.
//! Errors return a Kubernetes-style `Status`.
//!
//! Watches emit newline-delimited JSON documents.
//! Live output uses Server-Sent Events.
//! Both streams remain open until their source ends or fails.
//!
//! Every request body is limited to [`crate::MAX_REQUEST_BYTES`].

use std::{
    convert::Infallible,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, RawQuery, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderValue, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use solti_model::{
    LabelSelector, OutputEvent, TASK_API_VERSION, Task, TaskFilter, TaskManifest, TaskPhase,
    TaskQuery, TaskRun, TaskWatchEvent, Token, Uid, WritePreconditions,
};
use tokio_stream::{Stream, StreamExt};
use tower_http::limit::RequestBodyLimitLayer;
use tracing::debug;

use crate::{
    MAX_REQUEST_BYTES,
    auth::bearer_value,
    error::{ApiError, HttpStatusResource},
    handler::{ApiHandler, TaskWatchEventStream},
    metrics::{ApiMetricsHandle, StreamingResponse, http_metrics_middleware, noop_api_metrics},
    validate::{parse_list_limit, parse_task_id, validate_slot},
    visibility::{manifest_is_visible, run_is_visible, task_is_visible},
};
// `api_url!` is `#[macro_export]` and therefore already accessible in this
// module by its bare name — `use crate::api_url` would be redundant
// (and warnings about unused imports broke a `cargo publish` on us).

/// JSON extractor that maps axum rejections into [`ApiError`].
pub(crate) struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = axum::Json::<T>::from_request(req, state)
            .await
            .map_err(map_json_rejection)?;
        Ok(ApiJson(value))
    }
}

/// Query extractor that maps axum rejections into [`ApiError`].
pub(crate) struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Query(value) = Query::<T>::from_request_parts(parts, state)
            .await
            .map_err(map_query_rejection)?;
        Ok(ApiQuery(value))
    }
}

/// Path extractor that maps axum rejections into [`ApiError`].
pub(crate) struct ApiPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(value) = Path::<T>::from_request_parts(parts, state)
            .await
            .map_err(map_path_rejection)?;
        Ok(ApiPath(value))
    }
}

fn map_path_rejection(rejection: PathRejection) -> ApiError {
    ApiError::InvalidRequest(rejection.body_text())
}

fn map_query_rejection(rejection: QueryRejection) -> ApiError {
    ApiError::InvalidRequest(rejection.body_text())
}

fn map_json_rejection(rej: JsonRejection) -> ApiError {
    if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::PayloadTooLarge(format!(
            "request body exceeds the maximum of {} bytes",
            MAX_REQUEST_BYTES
        ));
    }
    if rej.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE {
        return ApiError::UnsupportedMediaType(rej.body_text());
    }

    let msg = rej.body_text();
    let trimmed = msg
        .strip_prefix("Failed to deserialize the JSON body into the target type: ")
        .or_else(|| msg.strip_prefix("Failed to parse the request body as JSON: "))
        .unwrap_or(&msg)
        .to_string();
    ApiError::InvalidRequest(trimmed)
}

async fn map_413_envelope(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    if resp.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::PayloadTooLarge(format!(
            "request body exceeds the maximum of {} bytes",
            MAX_REQUEST_BYTES
        ))
        .into_response();
    }
    resp
}

async fn route_not_found(req: Request) -> Response {
    ApiError::NotFound(format!("no API route for `{}`", req.uri().path())).into_response()
}

async fn method_not_allowed(req: Request) -> Response {
    ApiError::MethodNotAllowed(format!(
        "method {} is not allowed for `{}`",
        req.method(),
        req.uri().path()
    ))
    .into_response()
}

/// Builder for the axum task API.
///
/// Authentication and metrics are optional.
/// [`router`](Self::router) installs the complete route set and request limit.
///
/// ## Example
///
/// ```rust,no_run
/// use std::sync::Arc;
///
/// use solti_api::{ApiHandler, HttpApi};
/// use solti_model::Token;
///
/// fn build<H: ApiHandler>(
///     handler: Arc<H>,
///     token: Token,
/// ) -> solti_api::axum::Router {
///     HttpApi::new(handler)
///         .with_auth(token)
///         .router()
/// }
/// ```
///
/// ## See Also
///
/// - [`ApiHandler`] defines the backend operations.
/// - [`ApiError`] defines the HTTP `Status` mapping.
pub struct HttpApi<H> {
    handler: Arc<H>,
    metrics: ApiMetricsHandle,
    auth: Option<Token>,
}

impl<H> HttpApi<H>
where
    H: ApiHandler,
{
    /// Creates an HTTP API for one handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self {
            handler,
            metrics: noop_api_metrics(),
            auth: None,
        }
    }

    /// Requires a bearer token on every request.
    ///
    /// The expected header is `Authorization: Bearer <token>`.
    /// Missing or invalid credentials return `401 Unauthorized`.
    /// Rejected requests do not reach the handler.
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

    /// Builds the configured axum router.
    ///
    /// Every request body is limited to [`MAX_REQUEST_BYTES`].
    /// Optional authentication runs before the handler.
    /// Metrics include unmatched routes and authentication failures.
    pub fn router(self) -> Router {
        let mut router = Router::new()
            .route(api_url!("/tasks"), post(create_task::<H>))
            .route(api_url!("/tasks"), get(list_tasks::<H>))
            .route(api_url!("/tasks/{name}"), get(get_task::<H>))
            .route(api_url!("/tasks/{name}"), put(apply_task::<H>))
            .route(api_url!("/tasks/{name}"), delete(delete_task::<H>))
            .route(api_url!("/tasks/{name}/runs"), get(list_task_runs::<H>))
            .route(api_url!("/tasks/{name}/logs"), get(stream_task_logs::<H>))
            .fallback(route_not_found)
            .method_not_allowed_fallback(method_not_allowed)
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
            .layer(middleware::from_fn(map_413_envelope));

        // Added last → outermost → runs first: reject unauthenticated requests before any work happens.
        if let Some(token) = self.auth {
            router = router.layer(middleware::from_fn_with_state(token, require_bearer));
        }

        router = router.layer(middleware::from_fn_with_state(
            self.metrics,
            http_metrics_middleware,
        ));

        router.with_state(self.handler)
    }
}

/// Enforces the configured bearer token.
async fn require_bearer(State(expected): State<Token>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_value)
        .map(|presented| expected.verify(presented))
        .unwrap_or(false);

    if ok {
        next.run(req).await
    } else {
        ApiError::Unauthenticated("missing or invalid bearer token".into()).into_response()
    }
}

#[derive(Debug, Default)]
struct ListTasksParams {
    slot: Option<String>,
    phases: Vec<String>,
    label_selector: Option<String>,
    limit: Option<u32>,
    continuation: Option<String>,
    resource_version: Option<String>,
    watch: Option<bool>,
}

fn parse_list_tasks_params(raw_query: Option<&str>) -> Result<ListTasksParams, ApiError> {
    let mut params = ListTasksParams::default();
    for (key, value) in form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "slot" => set_query_param(&mut params.slot, "slot", value.into_owned())?,
            "phase" => params.phases.push(value.into_owned()),
            "labelSelector" => set_query_param(
                &mut params.label_selector,
                "labelSelector",
                value.into_owned(),
            )?,
            "limit" => {
                let value = parse_u32_query_param("limit", &value)?;
                set_query_param(&mut params.limit, "limit", value)?;
            }
            "continue" => {
                set_query_param(&mut params.continuation, "continue", value.into_owned())?
            }
            "resourceVersion" => set_query_param(
                &mut params.resource_version,
                "resourceVersion",
                value.into_owned(),
            )?,
            "watch" => {
                let value = parse_watch_query_param(&value)?;
                set_query_param(&mut params.watch, "watch", value)?;
            }
            other => {
                return Err(ApiError::InvalidRequest(format!(
                    "unknown query parameter `{other}`"
                )));
            }
        }
    }
    Ok(params)
}

fn parse_watch_query_param(value: &str) -> Result<bool, ApiError> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ApiError::InvalidRequest(
            "query parameter `watch` must be one of: true, false, 1, 0".into(),
        )),
    }
}

fn set_query_param<T>(target: &mut Option<T>, name: &str, value: T) -> Result<(), ApiError> {
    if target.replace(value).is_some() {
        return Err(ApiError::InvalidRequest(format!(
            "query parameter `{name}` must not be repeated"
        )));
    }
    Ok(())
}

fn parse_u32_query_param(name: &str, value: &str) -> Result<u32, ApiError> {
    value.parse().map_err(|_| {
        ApiError::InvalidRequest(format!(
            "query parameter `{name}` must be an unsigned 32-bit integer"
        ))
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteParams {
    uid: Option<String>,
    resource_version: Option<String>,
}

fn parse_write_preconditions(params: WriteParams) -> Result<WritePreconditions, ApiError> {
    let mut preconditions = WritePreconditions::new();
    if let Some(uid) = params.uid {
        preconditions = preconditions.with_uid(
            Uid::new(uid)
                .map_err(|error| ApiError::InvalidRequest(format!("invalid uid: {error}")))?,
        );
    }
    if let Some(resource_version) = params.resource_version {
        preconditions = preconditions
            .with_resource_version(resource_version)
            .map_err(|error| {
                ApiError::InvalidRequest(format!("invalid resourceVersion: {error}"))
            })?;
    }
    Ok(preconditions)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListMeta {
    resource_version: String,
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    continuation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remaining_item_count: Option<usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskList {
    api_version: &'static str,
    kind: &'static str,
    metadata: ListMeta,
    items: Vec<Task>,
}

#[derive(Debug, Serialize)]
struct TaskRunList {
    runs: Vec<TaskRun>,
}

#[derive(Serialize)]
#[serde(tag = "type", content = "object")]
enum TaskWatchDocument {
    #[serde(rename = "ADDED")]
    Added(Task),
    #[serde(rename = "MODIFIED")]
    Modified(Task),
    #[serde(rename = "DELETED")]
    Deleted(Task),
    #[serde(rename = "ERROR")]
    Error(HttpStatusResource),
}

struct TaskWatchBodyStream {
    source: TaskWatchEventStream,
    terminated: bool,
}

impl TaskWatchBodyStream {
    fn new(source: TaskWatchEventStream) -> Self {
        Self {
            source,
            terminated: false,
        }
    }
}

impl Stream for TaskWatchBodyStream {
    type Item = Result<Vec<u8>, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }

        match self.source.as_mut().poll_next(context) {
            Poll::Ready(Some(event)) => {
                let event = event.and_then(public_watch_event);
                self.terminated = event.is_err();
                Poll::Ready(Some(Ok(encode_watch_document(event))))
            }
            Poll::Ready(None) => {
                self.terminated = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn reject_embedded_manifest(manifest: &TaskManifest) -> Result<(), ApiError> {
    if !manifest_is_visible(manifest) {
        return Err(ApiError::InvalidRequest(
            "Embedded workloads are available only through the in-process SDK".into(),
        ));
    }
    Ok(())
}

fn public_task(task: Task) -> Result<Task, ApiError> {
    if !task_is_visible(&task) {
        return Err(ApiError::Internal(
            "handler returned an Embedded workload through the public HTTP API".into(),
        ));
    }
    Ok(task)
}

fn build_task_filter(
    slot: Option<String>,
    phases: Vec<String>,
    label_selector: Option<String>,
) -> Result<TaskFilter, ApiError> {
    let mut filter = TaskFilter::new();

    if let Some(slot) = slot {
        filter = filter.with_slot(validate_slot(slot)?);
    }

    for phase_str in phases {
        let phase = phase_str.parse::<TaskPhase>().map_err(|_| {
            ApiError::InvalidRequest(format!(
                "invalid phase: '{phase_str}' (valid: pending, running, succeeded, failed, timeout, canceled, exhausted)"
            ))
        })?;
        filter = filter.with_phase(phase);
    }

    if let Some(label_selector) = label_selector {
        let selector = label_selector
            .parse::<LabelSelector>()
            .map_err(|error| ApiError::InvalidRequest(format!("invalid labelSelector: {error}")))?;
        filter = filter
            .with_label_selector(selector)
            .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    }

    Ok(filter)
}

fn public_watch_event(event: TaskWatchEvent) -> Result<TaskWatchEvent, ApiError> {
    if !task_is_visible(event.object()) {
        return Err(ApiError::Internal(
            "handler returned an Embedded workload through the public HTTP watch".into(),
        ));
    }
    Ok(event)
}

fn encode_watch_document(event: Result<TaskWatchEvent, ApiError>) -> Vec<u8> {
    let document = match event {
        Ok(TaskWatchEvent::Added(task)) => TaskWatchDocument::Added(task),
        Ok(TaskWatchEvent::Modified(task)) => TaskWatchDocument::Modified(task),
        Ok(TaskWatchEvent::Deleted(task)) => TaskWatchDocument::Deleted(task),
        Err(error) => {
            let (_, status) = error.into_http_status();
            TaskWatchDocument::Error(status)
        }
    };

    match serde_json::to_vec(&document) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            tracing::error!(%error, "failed to serialize HTTP task watch event");
            br#"{"type":"ERROR","object":{"apiVersion":"v1","kind":"Status","metadata":{},"status":"Failure","message":"internal server error","reason":"InternalError","code":500}}
"#
            .to_vec()
        }
    }
}

async fn create_task<H>(
    State(handler): State<Arc<H>>,
    ApiJson(manifest): ApiJson<TaskManifest>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    reject_embedded_manifest(&manifest)?;
    debug!(name = %manifest.name(), "creating task");
    let task = public_task(handler.create_task(manifest).await?)?;
    Ok((StatusCode::CREATED, Json(task)))
}

async fn apply_task<H>(
    State(handler): State<Arc<H>>,
    ApiPath(path_name): ApiPath<String>,
    ApiQuery(params): ApiQuery<WriteParams>,
    ApiJson(manifest): ApiJson<TaskManifest>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let path_name = parse_task_id("task name", path_name)?;
    reject_embedded_manifest(&manifest)?;
    if manifest.name() != &path_name {
        return Err(ApiError::InvalidRequest(format!(
            "path task name `{path_name}` does not match metadata.name `{}`",
            manifest.name()
        )));
    }
    let preconditions = parse_write_preconditions(params)?;
    debug!(name = %manifest.name(), "applying task");
    let task = public_task(handler.apply_task(manifest, preconditions).await?)?;
    Ok(Json(task))
}

async fn get_task<H>(
    State(handler): State<Arc<H>>,
    ApiPath(name): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    debug!(%name, "getting task");
    let task = handler
        .get_task(&name)
        .await?
        .ok_or_else(|| ApiError::TaskNotFound(name.to_string()))?;

    Ok(Json(public_task(task)?))
}

async fn list_tasks<H>(
    State(handler): State<Arc<H>>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError>
where
    H: ApiHandler,
{
    let ListTasksParams {
        slot,
        phases,
        label_selector,
        limit,
        continuation,
        resource_version,
        watch,
    } = parse_list_tasks_params(raw_query.as_deref())?;
    let filter = build_task_filter(slot, phases, label_selector)?;

    if watch.unwrap_or(false) {
        if limit.is_some() || continuation.is_some() {
            return Err(ApiError::InvalidRequest(
                "query parameters `limit` and `continue` are not supported for watch".into(),
            ));
        }
        if resource_version
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ApiError::InvalidRequest(
                "query parameter `resourceVersion` must not be empty".into(),
            ));
        }

        let stream = handler.watch_tasks(filter, resource_version).await?;
        let body_stream = TaskWatchBodyStream::new(stream);
        let mut response = Body::from_stream(body_stream).into_response();
        response.extensions_mut().insert(StreamingResponse);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return Ok(response);
    }

    if resource_version.is_some() {
        return Err(ApiError::InvalidRequest(
            "query parameter `resourceVersion` requires `watch=true`".into(),
        ));
    }

    let mut query = TaskQuery::from_filter(filter);
    query = query.with_limit(parse_list_limit(limit.unwrap_or(0))?);
    if let Some(token) = continuation {
        if token.is_empty() {
            return Err(ApiError::InvalidRequest(
                "query parameter `continue` must not be empty".into(),
            ));
        }
        query = query.with_continuation(crate::continuation::decode(&token)?);
    }

    let page_filter = query.filter().clone();
    let page_limit = query.limit();
    let page = handler.query_tasks(query).await?;
    crate::continuation::validate_page(&page, &page_filter, page_limit)?;
    debug!(
        count = page.items.len(),
        remaining = page.remaining_item_count,
        "tasks listed"
    );

    for task in &page.items {
        if !task_is_visible(task) {
            return Err(ApiError::Internal(
                "handler returned an Embedded workload through the public HTTP API".into(),
            ));
        }
    }

    let continuation = page
        .continuation
        .map(crate::continuation::encode)
        .transpose()?;
    Ok(Json(TaskList {
        api_version: TASK_API_VERSION,
        kind: "TaskList",
        metadata: ListMeta {
            resource_version: page.resource_version,
            continuation,
            remaining_item_count: (page.remaining_item_count > 0)
                .then_some(page.remaining_item_count),
        },
        items: page.items,
    })
    .into_response())
}

async fn list_task_runs<H>(
    State(handler): State<Arc<H>>,
    ApiPath(name): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    debug!(%name, "listing task runs");
    let runs = handler.list_task_runs(&name).await?;
    if runs.iter().any(|run| !run_is_visible(run)) {
        return Err(ApiError::Internal(
            "handler returned Embedded run history through the public HTTP API".into(),
        ));
    }
    Ok(Json(TaskRunList { runs }))
}

async fn delete_task<H>(
    State(handler): State<Arc<H>>,
    ApiPath(name): ApiPath<String>,
    ApiQuery(params): ApiQuery<WriteParams>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    let preconditions = parse_write_preconditions(params)?;
    handler.delete_task(&name, preconditions).await?;
    debug!(%name, "task deleted");

    Ok(StatusCode::NO_CONTENT)
}

/// Streams task output as Server-Sent Events.
async fn stream_task_logs<H>(
    State(handler): State<Arc<H>>,
    ApiPath(name): ApiPath<String>,
) -> Result<Response, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    debug!(%name, "subscribing to task log stream");
    let stream = handler.stream_task_logs(&name).await?;

    let sse_stream = stream.map(|ev| {
        let name = match &ev {
            OutputEvent::Chunk(_) => "chunk",
            OutputEvent::RunStarted { .. } => "run-started",
            OutputEvent::RunFinished { .. } => "run-finished",
            OutputEvent::Lagged { .. } => "lagged",
            _ => "unknown",
        };
        let data = serde_json::to_string(&ev).map_err(|error| {
            ApiError::Internal(format!("failed to serialize output event: {error}"))
        })?;
        Ok::<Event, ApiError>(Event::default().event(name).data(data))
    });
    let mut response = Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    response.extensions_mut().insert(StreamingResponse);
    Ok(response)
}
