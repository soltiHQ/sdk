//! # HTTP/JSON transport.
//!
//! Axum router exposing [`ApiHandler`] operations as REST-shaped JSON endpoints.
//!
//! | Method | Endpoint                    | Handler             |
//! |--------|-----------------------------|---------------------|
//! | POST   | `/api/v1/tasks`             | submit              |
//! | GET    | `/api/v1/tasks`             | list (query params) |
//! | GET    | `/api/v1/tasks/{id}`        | get status          |
//! | GET    | `/api/v1/tasks/{id}/runs`   | list runs           |
//! | POST   | `/api/v1/tasks/{id}/cancel` | cancel              |
//! | DELETE | `/api/v1/tasks/{id}`        | delete              |

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::Deserialize;
use solti_model::{TaskId, TaskPhase, TaskQuery};
use tower_http::limit::RequestBodyLimitLayer;
use tracing::debug;

use crate::{
    convert::{self, clamp_list_limit, tasks_page_to_proto},
    error::ApiError,
    handler::ApiHandler,
    proto_api,
    validate::non_empty_id,
};

/// Maximum accepted JSON request body size (DoS defense).
const MAX_REQUEST_BODY_BYTES: usize = 256 * 1024;

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
    /// Applies a [`RequestBodyLimitLayer`] capped at
    /// [`MAX_REQUEST_BODY_BYTES`] bytes to every request.
    pub fn router(self) -> Router {
        Router::new()
            .route("/api/v1/tasks", post(submit_task::<H>))
            .route("/api/v1/tasks", get(list_tasks::<H>))
            .route("/api/v1/tasks/{id}", get(get_task_status::<H>))
            .route("/api/v1/tasks/{id}", delete(delete_task::<H>))
            .route("/api/v1/tasks/{id}/runs", get(list_task_runs::<H>))
            .route("/api/v1/tasks/{id}/cancel", post(cancel_task::<H>))
            .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
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
    Json(req): Json<proto_api::SubmitTaskRequest>,
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
    Ok((axum::http::StatusCode::CREATED, Json(response)))
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

    Ok(axum::http::StatusCode::NO_CONTENT)
}

async fn cancel_task<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    non_empty_id("task_id", &id)?;

    let task_id = TaskId::from(id);
    handler.cancel_task(&task_id).await?;
    debug!(%task_id, "task canceled");

    Ok(axum::http::StatusCode::NO_CONTENT)
}
