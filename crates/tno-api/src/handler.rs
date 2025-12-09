use async_trait::async_trait;
use tno_model::{CreateSpec, TaskId, TaskInfo};

use crate::error::ApiError;

/// Task execution API handler.
///
/// This trait abstracts the backend implementation, allowing users to:
/// - Use the provided `SupervisorApiAdapter`
/// - Implement custom handlers with additional logic (auth, rate limiting, etc.)
#[async_trait]
pub trait ApiHandler: Send + Sync + 'static {
    /// Submit a new task for execution.
    async fn submit_task(&self, spec: CreateSpec) -> Result<TaskId, ApiError>;

    /// Get current status of a task by ID.
    async fn get_task_status(&self, id: &TaskId) -> Result<Option<TaskInfo>, ApiError>;
}
