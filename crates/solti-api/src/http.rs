//! # HTTP/JSON transport.
//!
//! Axum router exposing [`ApiHandler`] operations as Kubernetes-shaped JSON endpoints.
//! All paths share the Kubernetes named-group prefix
//! `/apis/solti.io/v<MAJOR>` where `MAJOR` is [`crate::API_VERSION`];
//!
//! _the examples below show the current value (`v1`)_.
//!
//! | Method | Endpoint                              | Handler             |
//! |--------|---------------------------------------|---------------------|
//! | POST   | `/apis/solti.io/v1/tasks`             | create              |
//! | PUT    | `/apis/solti.io/v1/tasks/{name}`      | apply               |
//! | GET    | `/apis/solti.io/v1/tasks`             | list (query params) |
//! | GET    | `/apis/solti.io/v1/tasks/{name}`      | get                 |
//! | GET    | `/apis/solti.io/v1/tasks/{name}/runs` | list runs           |
//! | GET    | `/apis/solti.io/v1/tasks/{name}/logs` | live-tail SSE       |
//! | DELETE | `/apis/solti.io/v1/tasks/{name}`      | delete (stop+purge) |

use std::sync::Arc;

use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{StatusCode, request::Parts},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use solti_model::{
    OutputEvent, TASK_API_VERSION, Task, TaskManifest, TaskPhase, TaskQuery, TaskRun, Token,
};
use tokio_stream::StreamExt;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::debug;

use crate::{
    MAX_REQUEST_BYTES,
    auth::{assert_auth_token_not_empty, bearer_value},
    error::ApiError,
    handler::ApiHandler,
    validate::{clamp_list_limit, parse_task_id, validate_slot},
    visibility::{manifest_is_visible, run_is_visible, task_is_visible},
};
// `api_url!` is `#[macro_export]` and therefore already accessible in this
// module by its bare name — `use crate::api_url` would be redundant
// (and warnings about unused imports broke a `cargo publish` on us).

/// Wrapper around `axum::Json<T>` that maps `JsonRejection` into [`ApiError::InvalidRequest`].
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

/// Wrapper around `axum::extract::Query<T>` that maps query decoding failures into [`ApiError`].
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

/// Wrapper around `axum::extract::Path<T>` that maps path decoding failures
/// into the same structured error contract as every other extractor.
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

/// HTTP API service builder.
///
/// ## Also
///
/// - [`ApiHandler`](crate::ApiHandler) the trait backing all endpoints.
/// - [`ApiError`](crate::ApiError) mapped to JSON + HTTP status codes.
pub struct HttpApi<H> {
    handler: Arc<H>,
    auth: Option<Token>,
}

impl<H> HttpApi<H>
where
    H: ApiHandler,
{
    /// Create new HTTP API with the given handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self {
            handler,
            auth: None,
        }
    }

    /// Require a bearer token on every request.
    ///
    /// When set, requests without a valid `Authorization: Bearer <token>` header are rejected with `401 Unauthorized` before reaching any handler.
    /// This is the same shared secret the agent presents to the control plane in discovery.
    /// One config value enables both directions.
    /// Orthogonal to TLS. When unset, no auth is enforced.
    ///
    /// ## Panics
    ///
    /// Panics when `token` is empty: an empty shared secret would accept an empty
    /// bearer credential (`Authorization: Bearer `), silently disabling authentication.
    pub fn with_auth(mut self, token: Token) -> Self {
        assert_auth_token_not_empty(&token);
        self.auth = Some(token);
        self
    }

    /// Build axum router with mounted endpoints.
    ///
    /// Applies a [`RequestBodyLimitLayer`] capped at [`MAX_REQUEST_BYTES`] bytes to every request,
    /// and when [`with_auth`](Self::with_auth) is set a bearer-token gate that runs before any handler.
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

        router.with_state(self.handler)
    }
}

/// Axum middleware: reject requests lacking a valid `Authorization: Bearer` token.
/// Installed only when [`HttpApi::with_auth`] is set.
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

#[derive(Debug, Deserialize)]
struct ListTasksParams {
    slot: Option<String>,
    phase: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListMeta {
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
    debug!(name = %manifest.name(), "applying task");
    let task = public_task(handler.apply_task(manifest).await?)?;
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
    ApiQuery(params): ApiQuery<ListTasksParams>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let mut query = TaskQuery::new();

    if let Some(slot) = params.slot {
        query = query.with_slot(validate_slot(slot)?);
    }

    if let Some(phase_str) = params.phase {
        let phase = phase_str.parse::<TaskPhase>().map_err(|_| {
            ApiError::InvalidRequest(format!(
                "invalid phase: '{phase_str}' (valid: pending, running, succeeded, failed, timeout, canceled, exhausted)"
            ))
        })?;
        query = query.with_status(phase);
    }

    query = query.with_limit(clamp_list_limit(params.limit.unwrap_or(0)));
    let offset = params.offset.unwrap_or(0) as usize;
    if offset > 0 {
        query = query.with_offset(offset);
    }

    let page = handler.query_tasks(query).await?;
    debug!(count = page.items.len(), total = page.total, "tasks listed");

    for task in &page.items {
        if !task_is_visible(task) {
            return Err(ApiError::Internal(
                "handler returned an Embedded workload through the public HTTP API".into(),
            ));
        }
    }

    let remaining_item_count = page
        .total
        .saturating_sub(offset.saturating_add(page.items.len()));
    Ok(Json(TaskList {
        api_version: TASK_API_VERSION,
        kind: "TaskList",
        metadata: ListMeta {
            remaining_item_count: (remaining_item_count > 0).then_some(remaining_item_count),
        },
        items: page.items,
    }))
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
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let name = parse_task_id("task name", name)?;
    handler.delete_task(&name).await?;
    debug!(%name, "task deleted");

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /apis/solti.io/v1/tasks/{name}/logs` - Server-Sent Events stream of
/// [`OutputEvent`]s (live tail of stdout/stderr + run boundary markers + lag signals).
async fn stream_task_logs<H>(
    State(handler): State<Arc<H>>,
    ApiPath(name): ApiPath<String>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError>
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
        let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().event(name).data(data))
    });
    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}
