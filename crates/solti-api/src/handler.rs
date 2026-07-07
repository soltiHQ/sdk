//! # Handler trait.
//!
//! [`ApiHandler`] defines the transport-agnostic API surface.
//! Implement this trait to plug custom logic (auth, rate limiting, metrics) between the wire layer and the supervisor.

use std::pin::Pin;

use async_trait::async_trait;
use solti_model::{
    AdmissionPolicy, OutputEvent, Task, TaskId, TaskPage, TaskQuery, TaskRun, TaskSpec,
};
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
/// | `apply_task`       | `PUT    /api/v1/tasks`            | `ApplyTask`         |
/// | `get_task_status`  | `GET    /api/v1/tasks/{id}`       | `GetTaskStatus`     |
/// | `query_tasks`      | `GET    /api/v1/tasks`            | `ListTasks`         |
/// | `list_task_runs`   | `GET    /api/v1/tasks/{id}/runs`  | `ListTaskRuns`      |
/// | `delete_task`      | `DELETE /api/v1/tasks/{id}`       | `DeleteTask`        |
/// | `stream_task_logs` | `GET    /api/v1/tasks/{id}/logs`  | `StreamTaskLogs`    |
#[async_trait]
pub trait ApiHandler: Send + Sync + 'static {
    /// Submit a new task for execution.
    ///
    /// The spec's own admission policy decides what happens when the slot is busy.
    ///
    /// ## Errors
    ///
    /// The bundled [`SupervisorApiAdapter`](crate::SupervisorApiAdapter) returns:
    ///
    /// - [`ApiError::Core`]: the supervisor rejected the spec (invalid spec, no matching runner,
    ///   a non-terminal task with the same name, or an internal submit failure).
    ///
    /// Custom implementations may return other variants, e.g. [`ApiError::Internal`].
    async fn submit_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError>;

    /// Apply a spec to its slot (declarative upsert).
    /// Returns the id of the task running in the slot after apply.
    ///
    /// Note: this **forces** [`AdmissionPolicy::Replace`], overriding any admission
    /// policy on the supplied `spec` — that is the point of "apply" (latest spec
    /// wins the slot). Use [`submit_task`](Self::submit_task) to honor the spec's
    /// own admission policy.
    ///
    /// ## Errors
    ///
    /// Same as [`submit_task`](Self::submit_task); the default implementation forwards to it.
    async fn apply_task(&self, spec: TaskSpec) -> Result<TaskId, ApiError> {
        self.submit_task(spec.with_admission(AdmissionPolicy::Replace))
            .await
    }

    /// Get current status of a task by ID.
    ///
    /// Returns `Ok(None)` when no task with this id is known.
    ///
    /// ## Errors
    ///
    /// The bundled [`SupervisorApiAdapter`](crate::SupervisorApiAdapter) never fails here:
    /// a missing task is `Ok(None)`. Custom implementations may return any [`ApiError`].
    async fn get_task_status(&self, id: &TaskId) -> Result<Option<Task>, ApiError>;

    /// Query tasks with combined filters and pagination.
    ///
    /// Supports filtering by slot and/or status simultaneously, with offset/limit pagination. Returns a page with total count.
    ///
    /// ## Errors
    ///
    /// The bundled [`SupervisorApiAdapter`](crate::SupervisorApiAdapter) never fails here.
    /// Custom implementations may return any [`ApiError`].
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError>;

    /// List execution history for a specific task (oldest first).
    ///
    /// ## Errors
    ///
    /// The bundled [`SupervisorApiAdapter`](crate::SupervisorApiAdapter) never fails here:
    /// an unknown id yields an empty list. Custom implementations may return any [`ApiError`].
    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError>;

    /// Stop a task and purge its run history.
    ///
    /// Idempotent: returns `Ok(())` even when the task is not registered on the agent.
    ///
    /// ## Errors
    ///
    /// - [`ApiError::Core`]: the supervisor failed to cancel the bound run
    ///   (cancellation timeout or internal runtime failure).
    async fn delete_task(&self, id: &TaskId) -> Result<(), ApiError>;

    /// Subscribe to the live-tail stream of stdout/stderr lines for a task.
    ///
    /// Returns an [`OutputEventStream`] that yields [`OutputEvent`]s in real time.
    /// The stream covers all subsequent runs of the task (multi-run merge) and ends when the task is fully terminal and evicted.
    ///
    /// ## Errors
    ///
    /// - [`ApiError::TaskNotFound`]: no live output channel exists for this id
    ///   (bundled [`SupervisorApiAdapter`](crate::SupervisorApiAdapter)).
    ///
    /// The default implementation never fails; it returns a stream that ends immediately.
    async fn stream_task_logs(&self, _id: &TaskId) -> Result<OutputEventStream, ApiError> {
        Ok(Box::pin(tokio_stream::empty()))
    }
}
