//! # HTTP/JSON transport.
//!
//! Axum router exposing [`ApiHandler`] operations as REST-shaped JSON endpoints.
//! All paths share the `/api/v<MAJOR>` prefix where `MAJOR` is [`crate::API_VERSION`];
//!
//! _the examples below show the current value (`v1`)_.
//!
//! | Method | Endpoint                    | Handler             |
//! |--------|-----------------------------|---------------------|
//! | POST   | `/api/v1/tasks`             | submit              |
//! | GET    | `/api/v1/tasks`             | list (query params) |
//! | GET    | `/api/v1/tasks/{id}`        | get status          |
//! | GET    | `/api/v1/tasks/{id}/runs`   | list runs           |
//! | DELETE | `/api/v1/tasks/{id}`        | delete (stop+purge) |

use std::sync::Arc;

use std::convert::Infallible;

use axum::{
    Json, Router,
    extract::{FromRequest, Path, Query, Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post},
};
use serde::{Deserialize, de::DeserializeOwned};
use solti_model::{OutputEvent, TaskId, TaskPhase, TaskQuery};
use tokio_stream::StreamExt;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::debug;

use crate::{
    MAX_REQUEST_BYTES,
    convert::{self, tasks_page_to_proto},
    error::ApiError,
    handler::ApiHandler,
    proto_api,
    validate::{clamp_list_limit, non_empty_id},
};
// `api_url!` is `#[macro_export]`, so it's already accessible in this
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

fn map_json_rejection(rej: JsonRejection) -> ApiError {
    if rej.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::PayloadTooLarge(format!(
            "request body exceeds the maximum of {} bytes",
            MAX_REQUEST_BYTES
        ));
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
        let body = serde_json::json!({
            "error": "PayloadTooLarge",
            "message": format!(
                "request body exceeds the maximum of {} bytes",
                MAX_REQUEST_BYTES
            ),
        });
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(body)).into_response();
    }
    resp
}

/// HTTP API service builder.
///
/// ## Also
///
/// - [`ApiHandler`](crate::ApiHandler) the trait backing all endpoints.
/// - [`ApiError`](crate::ApiError) mapped to JSON + HTTP status codes.
pub struct HttpApi<H> {
    handler: Arc<H>,
}

impl<H> HttpApi<H>
where
    H: ApiHandler,
{
    /// Create new HTTP API with the given handler.
    pub fn new(handler: Arc<H>) -> Self {
        Self { handler }
    }

    /// Build axum router with mounted endpoints.
    ///
    /// Applies a [`RequestBodyLimitLayer`] capped at [`MAX_REQUEST_BYTES`] bytes to every request.
    pub fn router(self) -> Router {
        Router::new()
            .route(api_url!("/tasks"), post(submit_task::<H>))
            .route(api_url!("/tasks"), get(list_tasks::<H>))
            .route(api_url!("/tasks/{id}"), get(get_task_status::<H>))
            .route(api_url!("/tasks/{id}"), delete(delete_task::<H>))
            .route(api_url!("/tasks/{id}/runs"), get(list_task_runs::<H>))
            .route(api_url!("/tasks/{id}/logs"), get(stream_task_logs::<H>))
            .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BYTES))
            .layer(middleware::from_fn(map_413_envelope))
            .with_state(self.handler)
    }
}

#[derive(Debug, Deserialize)]
struct ListTasksParams {
    slot: Option<String>,
    status: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn submit_task<H>(
    State(handler): State<Arc<H>>,
    ApiJson(req): ApiJson<proto_api::SubmitTaskRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let spec = req
        .spec
        .ok_or_else(|| ApiError::InvalidRequest("missing spec".into()))?;
    let spec = convert::convert_create_spec(spec)?;

    debug!(slot = %spec.slot(), kind = ?spec.kind(), "submitting task");
    let task_id = handler.submit_task(spec).await?;

    let response = proto_api::SubmitTaskResponse {
        task_id: task_id.to_string(),
    };
    Ok((StatusCode::CREATED, Json(response)))
}

async fn get_task_status<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    non_empty_id("task_id", &id)?;

    let task_id = TaskId::from(id);
    debug!(%task_id, "getting task status");
    let task = handler.get_task_status(&task_id).await?;

    let task = task.map(proto_api::TaskData::try_from).transpose()?;
    Ok(Json(proto_api::GetTaskStatusResponse { task }))
}

async fn list_tasks<H>(
    State(handler): State<Arc<H>>,
    Query(params): Query<ListTasksParams>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let mut query = TaskQuery::new();

    if let Some(slot) = params.slot {
        non_empty_id("slot", &slot)?;
        query = query.with_slot(slot);
    }

    if let Some(status_str) = params.status {
        let status = status_str.parse::<TaskPhase>().map_err(|_| {
            ApiError::InvalidRequest(format!(
                "invalid status: '{status_str}' (valid: pending, running, succeeded, failed, timeout, canceled, exhausted)"
            ))
        })?;
        query = query.with_status(status);
    }

    query = query.with_limit(clamp_list_limit(params.limit.unwrap_or(0)));
    if let Some(offset) = params.offset {
        query = query.with_offset(offset as usize);
    }

    let page = handler.query_tasks(query).await?;
    debug!(count = page.items.len(), total = page.total, "tasks listed");

    Ok(Json(tasks_page_to_proto(page)?))
}

async fn list_task_runs<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    non_empty_id("task_id", &id)?;

    let task_id = TaskId::from(id);
    debug!(%task_id, "listing task runs");
    let runs = handler.list_task_runs(&task_id).await?;
    let runs = runs.into_iter().map(proto_api::TaskRunInfo::from).collect();

    Ok(Json(proto_api::ListTaskRunsResponse { runs }))
}

async fn delete_task<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    non_empty_id("task_id", &id)?;

    let task_id = TaskId::from(id);
    handler.delete_task(&task_id).await?;
    debug!(%task_id, "task deleted");

    Ok(StatusCode::NO_CONTENT)
}

/// `GET /tasks/{id}/logs` - Server-Sent Events stream of [`OutputEvent`]s (live tail of stdout/stderr + run boundary markers + lag signals).
async fn stream_task_logs<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    H: ApiHandler,
{
    non_empty_id("task_id", &id)?;

    let task_id = TaskId::from(id);
    debug!(%task_id, "subscribing to task log stream");
    let stream = handler.stream_task_logs(&task_id).await?;

    let sse_stream = stream.map(|ev| {
        let name = match &ev {
            OutputEvent::Chunk(_) => "chunk",
            OutputEvent::RunStarted { .. } => "run-started",
            OutputEvent::RunFinished { .. } => "run-finished",
            OutputEvent::Lagged { .. } => "lagged",
        };
        let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
        Ok(Event::default().event(name).data(data))
    });
    Ok(Sse::new(sse_stream).keep_alive(KeepAlive::default()))
}
