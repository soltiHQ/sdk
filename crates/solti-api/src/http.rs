//! # HTTP/JSON transport.
//!
//! [`HttpApi`] builds an axum `Router` with JSON endpoints matching the gRPC API surface.
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
use std::time::UNIX_EPOCH;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use solti_model::{Task, TaskId, TaskPhase, TaskQuery, TaskRun, TaskSpec};
use tracing::{debug, warn};

use crate::{error::ApiError, handler::ApiHandler};

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
    /// Routes:
    /// - `POST   /api/v1/tasks`             — Submit task
    /// - `GET    /api/v1/tasks`             — Query tasks (filters + pagination)
    /// - `GET    /api/v1/tasks/{id}`        — Get task status
    /// - `GET    /api/v1/tasks/{id}/runs`   — List task execution history
    /// - `POST   /api/v1/tasks/{id}/cancel` — Cancel a running task
    /// - `DELETE /api/v1/tasks/{id}`        — Delete task and its runs
    pub fn router(self) -> Router {
        Router::new()
            .route("/api/v1/tasks", post(submit_task::<H>))
            .route("/api/v1/tasks", get(list_tasks::<H>))
            .route("/api/v1/tasks/{id}", get(get_task_status::<H>))
            .route("/api/v1/tasks/{id}", delete(delete_task::<H>))
            .route("/api/v1/tasks/{id}/runs", get(list_task_runs::<H>))
            .route("/api/v1/tasks/{id}/cancel", post(cancel_task::<H>))
            .with_state(self.handler)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitTaskRequest {
    spec: TaskSpec,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitTaskResponse {
    task_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GetTaskStatusResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<TaskInfoWire>,
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
    tasks: Vec<TaskInfoWire>,
    total: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListTaskRunsResponse {
    runs: Vec<TaskRunWire>,
}

/// Flat task representation matching proto `TaskInfo`.
#[derive(Debug, Serialize, Deserialize)]
struct TaskInfoWire {
    id: String,
    slot: String,
    status: String,
    attempt: u32,
    created_at: i64,
    updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    generation: u64,
    resource_version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

impl From<Task> for TaskInfoWire {
    fn from(task: Task) -> Self {
        let created_at = task
            .metadata
            .created_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|e| {
                warn!(task_id = %task.metadata.id, error = %e, "created_at before epoch");
                std::time::Duration::ZERO
            })
            .as_millis() as i64;

        let updated_at = task
            .metadata
            .updated_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|e| {
                warn!(task_id = %task.metadata.id, error = %e, "updated_at before epoch");
                std::time::Duration::ZERO
            })
            .as_millis() as i64;

        Self {
            id: task.metadata.id.to_string(),
            slot: task.slot().to_string(),
            status: phase_to_string(&task.status.phase),
            attempt: task.status.attempt,
            created_at,
            updated_at,
            error: task.status.error,
            generation: task.metadata.generation,
            resource_version: task.metadata.resource_version,
            exit_code: task.status.exit_code,
        }
    }
}

/// Flat task run representation matching proto `TaskRunInfo`.
#[derive(Debug, Serialize, Deserialize)]
struct TaskRunWire {
    attempt: u32,
    status: String,
    started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
}

impl From<TaskRun> for TaskRunWire {
    fn from(run: TaskRun) -> Self {
        let started_at = run
            .started_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let finished_at = run
            .finished_at
            .map(|t| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64);

        Self {
            attempt: run.attempt,
            status: phase_to_string(&run.phase),
            started_at,
            finished_at,
            error: run.error,
            exit_code: run.exit_code,
        }
    }
}

fn phase_to_string(phase: &TaskPhase) -> String {
    match phase {
        TaskPhase::Pending => "pending",
        TaskPhase::Running => "running",
        TaskPhase::Succeeded => "succeeded",
        TaskPhase::Failed => "failed",
        TaskPhase::Timeout => "timeout",
        TaskPhase::Canceled => "canceled",
        TaskPhase::Exhausted => "exhausted",
        other => {
            warn!(?other, "unknown TaskPhase variant, mapping to unknown");
            "unknown"
        }
    }
    .to_string()
}

/// POST /api/v1/tasks
async fn submit_task<H>(
    State(handler): State<Arc<H>>,
    Json(req): Json<SubmitTaskRequest>,
) -> Result<impl IntoResponse, ApiError>
where
    H: ApiHandler,
{
    req.spec
        .validate()
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    debug!(slot = %req.spec.slot(), kind = ?req.spec.kind(), "submitting task");
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
    if id.trim().is_empty() {
        return Err(ApiError::InvalidRequest("task_id cannot be empty".into()));
    }

    let task_id = TaskId::from(id);
    debug!(%task_id, "getting task status");
    let info = handler.get_task_status(&task_id).await?;

    let response = GetTaskStatusResponse {
        info: info.map(TaskInfoWire::from),
    };

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
        tasks: page.items.into_iter().map(TaskInfoWire::from).collect(),
        total: page.total,
    };
    Ok(Json(response))
}

/// Parse TaskPhase from string.
fn parse_status(s: &str) -> Result<TaskPhase, ApiError> {
    match s.to_lowercase().as_str() {
        "pending" => Ok(TaskPhase::Pending),
        "running" => Ok(TaskPhase::Running),
        "succeeded" => Ok(TaskPhase::Succeeded),
        "failed" => Ok(TaskPhase::Failed),
        "timeout" => Ok(TaskPhase::Timeout),
        "canceled" => Ok(TaskPhase::Canceled),
        "exhausted" => Ok(TaskPhase::Exhausted),
        _ => Err(ApiError::InvalidRequest(format!(
            "invalid status: '{}' (valid: pending, running, succeeded, failed, timeout, canceled, exhausted)",
            s
        ))),
    }
}

/// GET /api/v1/tasks/:id/runs
async fn list_task_runs<H>(
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
    debug!(%task_id, "listing task runs");
    let runs = handler.list_task_runs(&task_id).await?;

    Ok(Json(ListTaskRunsResponse {
        runs: runs.into_iter().map(TaskRunWire::from).collect(),
    }))
}

/// DELETE /api/v1/tasks/:id
async fn delete_task<H>(
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
    handler.delete_task(&task_id).await?;
    debug!(%task_id, "task deleted");

    Ok(axum::http::StatusCode::NO_CONTENT)
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
