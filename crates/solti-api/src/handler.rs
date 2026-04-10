//! # Handler trait.
//!
//! [`ApiHandler`] defines the transport-agnostic API surface.
//! Implement this trait to plug custom logic (auth, rate limiting, metrics) between the wire layer and the supervisor.

use async_trait::async_trait;
use solti_model::{Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec};

use crate::error::ApiError;

/// Task execution API handler.
///
/// ## Also
///
/// - [`SupervisorApiAdapter`](crate::SupervisorApiAdapter) ready-to-use implementation.
/// - [`ApiError`](crate::ApiError) error type returned by all methods.
///
/// This trait abstracts the backend implementation, allowing users to:
/// - Use the provided [`SupervisorApiAdapter`](crate::SupervisorApiAdapter)
/// - Implement custom handlers with additional logic (auth, rate limiting, etc.)
///
/// ## API surface
///
/// | Method             | HTTP                              | gRPC                |
/// |--------------------|-----------------------------------|---------------------|
/// | `submit_task`      | `POST   /api/v1/tasks`            | `SubmitTask`        |
/// | `get_task_status`  | `GET    /api/v1/tasks/{id}`       | `GetTaskStatus`     |
/// | `query_tasks`      | `GET    /api/v1/tasks`            | `ListTasks`         |
/// | `list_task_runs`   | `GET    /api/v1/tasks/{id}/runs`  | `ListTaskRuns`      |
/// | `cancel_task`      | `POST   /api/v1/tasks/{id}/cancel`| `CancelTask`        |
/// | `delete_task`      | `DELETE /api/v1/tasks/{id}`       | `DeleteTask`        |
#[async_trait]
pub trait ApiHandler: Send + Sync + 'static {
    /// Submit a new task for execution.
    async fn submit_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError>;

    /// Get current status of a task by ID.
    async fn get_task_status(&self, id: &TaskId) -> Result<Option<Task>, ApiError>;

    /// Query tasks with combined filters and pagination.
    ///
    /// Supports filtering by slot and/or status simultaneously, with offset/limit pagination. Returns a page with total count.
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError>;

    /// List execution history for a specific task (oldest first).
    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError>;

    /// Cancel a running task.
    ///
    /// Sends cancellation signal to the task. The task must cooperate by checking its `CancellationToken`.
    async fn cancel_task(&self, id: &TaskId) -> Result<(), ApiError>;

    /// Delete a task and its associated run history.
    ///
    /// Returns `TaskNotFound` if the task does not exist.
    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError>;
}
