use async_trait::async_trait;
use solti_model::{Task, TaskId, TaskPage, TaskPhase, TaskQuery, TaskSpec};

use crate::error::ApiError;

/// Task execution API handler.
///
/// This trait abstracts the backend implementation, allowing users to:
/// - Use the provided `SupervisorApiAdapter`
/// - Implement custom handlers with additional logic (auth, rate limiting, etc.)
#[async_trait]
pub trait ApiHandler: Send + Sync + 'static {
    /// Submit a new task for execution.
    async fn submit_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError>;

    /// Get current status of a task by ID.
    async fn get_task_status(&self, id: &TaskId) -> Result<Option<Task>, ApiError>;

    /// List all tasks.
    async fn list_all_tasks(&self) -> Result<Vec<Task>, ApiError>;

    /// List tasks in a specific slot.
    async fn list_tasks_by_slot(&self, slot: &str) -> Result<Vec<Task>, ApiError>;

    /// List tasks by phase.
    async fn list_tasks_by_status(&self, status: TaskPhase) -> Result<Vec<Task>, ApiError>;

    /// Query tasks with combined filters and pagination.
    ///
    /// Supports filtering by slot and/or status simultaneously,
    /// with offset/limit pagination. Returns a page with total count.
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError>;

    /// Cancel a running task.
    ///
    /// Sends cancellation signal to the task. The task must cooperate
    /// by checking its `CancellationToken`.
    async fn cancel_task(&self, id: &TaskId) -> Result<(), ApiError>;
}
