//! # Handler trait.
//!
//! [`ApiHandler`] defines the transport-agnostic API surface.
//! Implement this trait to plug custom logic (auth, rate limiting, metrics) between the wire layer and the supervisor.

use std::pin::Pin;

use async_trait::async_trait;
use solti_model::{OutputEvent, Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec};
use tokio_stream::Stream;

use crate::error::ApiError;

/// Boxed stream of [`OutputEvent`]s — the wire-side surface of live task logs.
pub type OutputEventStream = Pin<Box<dyn Stream<Item = OutputEvent> + Send + 'static>>;

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
/// | `delete_task`      | `DELETE /api/v1/tasks/{id}`       | `DeleteTask`        |
/// | `stream_task_logs` | `GET    /api/v1/tasks/{id}/logs`  | `StreamTaskLogs`    |
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

    /// Stop a task and purge its run history.
    ///
    /// Idempotent:
    /// returns `Ok(())` whether the task is currently registered on the agent.
    /// Errors only on supervisor cancellation failures (timeout, internal error).
    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError>;

    /// Subscribe to the live-tail stream of stdout/stderr lines for a task.
    ///
    /// Returns an [`OutputEventStream`] that yields [`OutputEvent`]s in real time.
    /// The stream covers all subsequent runs of the task (multi-run merge) and ends when the task is fully terminal and evicted.
    async fn stream_task_logs(&self, _id: &TaskId) -> Result<OutputEventStream, ApiError> {
        Ok(Box::pin(tokio_stream::empty()))
    }
}
