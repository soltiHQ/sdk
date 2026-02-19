use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use solti_model::{CreateSpec, TaskId, TaskInfo, TaskQuery, TaskStatus};
use tracing::debug;

use crate::{error::ApiError, handler::ApiHandler};

/// HTTP API service builder.
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
    /// Routes:
    /// - POST /api/v1/tasks - Submit task
    /// - GET /api/v1/tasks/:id - Get task status
    /// - GET /api/v1/tasks - List all tasks (or filter by query params)
    pub fn router(self) -> Router {
        Router::new()
            .route("/api/v1/tasks", post(submit_task::<H>))
            .route("/api/v1/tasks", get(list_tasks::<H>))
            .route("/api/v1/tasks/{id}", get(get_task_status::<H>))
            .route("/api/v1/tasks/{id}/cancel", post(cancel_task::<H>)) // НОВОЕ
            .with_state(self.handler)
    }
}

// ============================================================================
// Request/Response types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct SubmitTaskRequest {
    spec: CreateSpec,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitTaskResponse {
    task_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GetTaskStatusResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<TaskInfo>,
}

#[derive(Debug, Deserialize)]
struct ListTasksParams {
    /// Filter by slot name
    slot: Option<String>,
    /// Filter by task status
    status: Option<String>,
    /// Max items per page (default 100, max 1000)
    limit: Option<usize>,
    /// Offset for pagination (default 0)
    offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListTasksResponse {
    tasks: Vec<TaskInfo>,
    total: usize,
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /api/v1/tasks
async fn submit_task<H>(
    State(handler): State<Arc<H>>,
    Json(req): Json<SubmitTaskRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    debug!(slot = %req.spec.slot, kind = ?req.spec.kind, "submitting task");
    let task_id = handler.submit_task(req.spec).await?;

    let response = SubmitTaskResponse {
        task_id: task_id.to_string(),
    };

    Ok((axum::http::StatusCode::CREATED, Json(response)))
}

/// GET /api/v1/tasks/:id
async fn get_task_status<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let task_id = TaskId::from(id);
    debug!(%task_id, "getting task status");
    let info = handler.get_task_status(&task_id).await?;

    let response = GetTaskStatusResponse { info };

    Ok(Json(response))
}

/// GET /api/v1/tasks
///
/// Query params (all optional, combinable):
/// - ?slot=name    - filter by slot
/// - ?status=running - filter by status
/// - ?limit=50     - max items per page (default 100, max 1000)
/// - ?offset=0     - pagination offset (default 0)
async fn list_tasks<H>(
    State(handler): State<Arc<H>>,
    Query(params): Query<ListTasksParams>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let mut query = TaskQuery::new();

    if let Some(slot) = params.slot {
        if slot.trim().is_empty() {
            return Err(ApiError::InvalidRequest("slot cannot be empty".into()));
        }
        query = query.with_slot(slot);
    }

    if let Some(status_str) = params.status {
        let status = parse_status(&status_str)?;
        query = query.with_status(status);
    }

    if let Some(limit) = params.limit {
        query = query.with_limit(limit);
    }

    if let Some(offset) = params.offset {
        query = query.with_offset(offset);
    }

    let page = handler.query_tasks(query).await?;
    debug!(count = page.items.len(), total = page.total, "tasks listed");

    let response = ListTasksResponse {
        tasks: page.items,
        total: page.total,
    };
    Ok(Json(response))
}

/// Parse TaskStatus from string.
fn parse_status(s: &str) -> Result<TaskStatus, ApiError> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(TaskStatus::Pending),
        "running" => Ok(TaskStatus::Running),
        "succeeded" => Ok(TaskStatus::Succeeded),
        "failed" => Ok(TaskStatus::Failed),
        "timeout" => Ok(TaskStatus::Timeout),
        "canceled" => Ok(TaskStatus::Canceled),
        "exhausted" => Ok(TaskStatus::Exhausted),
        _ => Err(ApiError::InvalidRequest(format!(
            "invalid status: '{}' (valid: pending, running, succeeded, failed, timeout, canceled, exhausted)",
            s
        ))),
    }
}

/// POST /api/v1/tasks/:id/cancel
async fn cancel_task<H>(
    State(handler): State<Arc<H>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    if id.trim().is_empty() {
        return Err(ApiError::InvalidRequest("task_id cannot be empty".into()));
    }

    let task_id = TaskId::from(id);
    handler.cancel_task(&task_id).await?;
    debug!(%task_id, "task canceled");

    Ok(axum::http::StatusCode::NO_CONTENT)
}
