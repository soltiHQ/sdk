use async_trait::async_trait;
use solti_model::{CreateSpec, TaskId, TaskInfo, TaskPage, TaskQuery, TaskStatus};

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

    /// List all tasks.
    async fn list_all_tasks(&self) -> Result<Vec<TaskInfo>, ApiError>;

    /// List tasks in a specific slot.
    async fn list_tasks_by_slot(&self, slot: &str) -> Result<Vec<TaskInfo>, ApiError>;

    /// List tasks by status.
    async fn list_tasks_by_status(&self, status: TaskStatus) -> Result<Vec<TaskInfo>, ApiError>;

    /// Query tasks with combined filters and pagination.
    ///
    /// Supports filtering by slot and/or status simultaneously,
    /// with offset/limit pagination. Returns a page with total count.
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<TaskInfo>, ApiError>;

    /// Cancel a running task.
    ///
    /// Sends cancellation signal to the task. The task must cooperate
    /// by checking its `CancellationToken`.
    async fn cancel_task(&self, id: &TaskId) -> Result<(), ApiError>;
}
