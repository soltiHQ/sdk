use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tno_model::{CreateSpec, TaskId, TaskInfo};

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
    pub fn router(self) -> Router {
        Router::new()
            .route("/api/v1/tasks", post(submit_task::<H>))
            .route("/api/v1/tasks/{id}", get(get_task_status::<H>))
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
