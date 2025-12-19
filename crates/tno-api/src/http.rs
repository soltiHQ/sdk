use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tno_model::{CreateSpec, TaskId, TaskInfo, TaskStatus};

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
struct ListTasksQuery {
    /// Filter by slot name
    slot: Option<String>,
    /// Filter by task status
    status: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListTasksResponse {
    tasks: Vec<TaskInfo>,
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
    let task_id = handler.submit_task(req.spec).await?;

    let response = SubmitTaskResponse {
        task_id: task_id.to_string(),
    };

    Ok(Json(response))
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
    let info = handler.get_task_status(&task_id).await?;

    let response = GetTaskStatusResponse { info };

    Ok(Json(response))
}

/// GET /api/v1/tasks
///
/// Query params:
/// - ?slot=name - filter by slot
/// - ?status=running - filter by status
/// - no params - list all tasks
async fn list_tasks<H>(
    State(handler): State<Arc<H>>,
    Query(query): Query<ListTasksQuery>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    let tasks = match (query.slot, query.status) {
        // Filter by slot
        (Some(slot), None) => {
            if slot.trim().is_empty() {
                return Err(ApiError::InvalidRequest("slot cannot be empty".into()));
            }
            handler.list_tasks_by_slot(&slot).await?
        }
        // Filter by status
        (None, Some(status_str)) => {
            let status = parse_status(&status_str)?;
            handler.list_tasks_by_status(status).await?
        }
        // Both filters - not supported
        (Some(_), Some(_)) => {
            return Err(ApiError::InvalidRequest(
                "cannot filter by both slot and status simultaneously".into(),
            ));
        }
        // No filters - list all
        (None, None) => handler.list_all_tasks().await?,
    };

    let response = ListTasksResponse { tasks };

    Ok(Json(response))
}

/// Parse TaskStatus from string
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

    Ok(axum::http::StatusCode::NO_CONTENT)
}
