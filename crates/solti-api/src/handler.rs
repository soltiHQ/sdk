//! # Handler trait.
//!
//! [`ApiHandler`] defines the transport-agnostic API surface.
//! Implement this trait to plug custom logic (auth, rate limiting, metrics) between the wire layer and the supervisor.

use std::pin::Pin;

use async_trait::async_trait;
use solti_model::{
    OutputEvent, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRun,
    TaskWatchEvent, WritePreconditions,
};
use tokio_stream::Stream;

use crate::error::ApiError;

/// Boxed stream of [`OutputEvent`]s — the wire-side surface of live task logs.
pub type OutputEventStream = Pin<Box<dyn Stream<Item = OutputEvent> + Send + 'static>>;

/// Boxed stream of Task resource changes.
pub type TaskWatchEventStream =
    Pin<Box<dyn Stream<Item = Result<TaskWatchEvent, ApiError>> + Send + 'static>>;

/// Task execution API handler.
///
/// ## Also
///
/// - `SupervisorApiAdapter` ready-to-use implementation (feature `core-adapter`).
/// - [`ApiError`](crate::ApiError) error type returned by all methods.
///
/// This trait abstracts the backend implementation, allowing users to:
/// - Use the provided `SupervisorApiAdapter` with feature `core-adapter`.
/// - Implement custom handlers with additional logic (auth, rate limiting, etc.)
///
/// ## API surface
///
/// | Method             | HTTP                                         | gRPC             |
/// |--------------------|----------------------------------------------|------------------|
/// | `create_task`      | `POST   /apis/solti.io/v1/tasks`             | `CreateTask`     |
/// | `apply_task`       | `PUT    /apis/solti.io/v1/tasks/{name}`      | `ApplyTask`      |
/// | `get_task`         | `GET    /apis/solti.io/v1/tasks/{name}`      | `GetTask`        |
/// | `query_tasks`      | `GET    /apis/solti.io/v1/tasks`             | `ListTasks`      |
/// | `watch_tasks`      | `GET    /apis/solti.io/v1/tasks?watch=true`  | `WatchTasks`     |
/// | `list_task_runs`   | `GET    /apis/solti.io/v1/tasks/{name}/runs` | `ListTaskRuns`   |
/// | `delete_task`      | `DELETE /apis/solti.io/v1/tasks/{name}`      | `DeleteTask`     |
/// | `stream_task_logs` | `GET    /apis/solti.io/v1/tasks/{name}/logs` | `StreamTaskLogs` |
#[async_trait]
pub trait ApiHandler: Send + Sync + 'static {
    /// Create a named Task resource.
    ///
    /// The bundled `SupervisorApiAdapter` returns the committed desired resource
    /// immediately. Runtime reconciliation continues in the background; clients
    /// observe it through the `status.conditions[type=Reconciled]` condition.
    ///
    /// ## Errors
    ///
    /// The bundled `SupervisorApiAdapter` returns:
    ///
    /// - [`ApiError::InvalidRequest`] when admission rejects the manifest;
    /// - [`ApiError::AlreadyExists`] when a retained resource owns the same name;
    /// - [`ApiError::Unavailable`] when shutdown has started.
    ///
    /// Runner, mapping, and Taskvisor reconciliation failures happen after the
    /// desired-state commit and are recorded in the returned resource's later
    /// `status`; they are not request errors.
    ///
    /// Custom implementations may return other variants, e.g. [`ApiError::Internal`].
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError>;

    /// Declaratively create or update the resource addressed by `metadata.name`.
    ///
    /// Empty preconditions preserve upsert semantics. Non-empty preconditions
    /// require an existing matching resource.
    ///
    /// ## Errors
    ///
    /// Same categories as [`create_task`](Self::create_task), plus:
    ///
    /// - [`ApiError::TaskNotFound`] when conditional apply targets no resource;
    /// - [`ApiError::Conflict`] when a precondition does not match.
    async fn apply_task(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
    ) -> Result<Task, ApiError>;

    /// Get a current task resource by name.
    ///
    /// Returns `Ok(None)` when no task with this id is known.
    ///
    /// ## Errors
    ///
    /// The bundled `SupervisorApiAdapter` never fails here:
    /// a missing task is `Ok(None)`. Custom implementations may return any [`ApiError`].
    async fn get_task(&self, name: &TaskId) -> Result<Option<Task>, ApiError>;

    /// Query tasks with combined filters and snapshot-consistent continuation pagination.
    ///
    /// ## Errors
    ///
    /// The bundled `SupervisorApiAdapter` returns [`ApiError::InvalidRequest`]
    /// for an invalid continuation and [`ApiError::ResourceVersionExpired`]
    /// when its snapshot is no longer retained.
    /// Custom implementations may return any [`ApiError`].
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError>;

    /// Watch changes to tasks matching the filter.
    ///
    /// An absent resource version or `"0"` starts with `Added` events for the
    /// current matching resources. A specific version replays later retained
    /// changes, then continues with live events.
    ///
    /// ## Errors
    ///
    /// - [`ApiError::ResourceVersionExpired`]: the requested position is no
    ///   longer retained.
    ///
    /// Streams may later yield the same error when a subscriber falls behind
    /// the retained history.
    async fn watch_tasks(
        &self,
        filter: TaskFilter,
        resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError>;

    /// List execution history for a specific task (oldest first).
    ///
    /// ## Errors
    ///
    /// - [`ApiError::TaskNotFound`]: the task is absent from the public API.
    ///
    /// Custom implementations may return any [`ApiError`].
    async fn list_task_runs(&self, id: &TaskId) -> Result<Vec<TaskRun>, ApiError>;

    /// Stop a task and purge its run history.
    ///
    /// ## Errors
    ///
    /// - [`ApiError::TaskNotFound`]: the task is absent from the public API;
    /// - [`ApiError::Conflict`]: a precondition does not match;
    /// - [`ApiError::Internal`]: the supervisor failed to cancel the bound submission,
    ///   whether registered or controller-queued (timeout or internal runtime failure).
    async fn delete_task(
        &self,
        id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError>;

    /// Subscribe to the live-tail stream of stdout/stderr lines for a task.
    ///
    /// Returns a lossy, live-only [`OutputEventStream`] that yields
    /// [`OutputEvent`]s in real time without persistence or replay. It can cover
    /// subsequent runs of the task (multi-run merge); lifecycle boundary events
    /// are best-effort observations, not ordering barriers for output chunks.
    /// Terminal cleanup removes the core hub sender; an already-open stream
    /// closes after any outstanding runner-owned output-sink clones are also
    /// dropped.
    ///
    /// ## Errors
    ///
    /// - [`ApiError::TaskNotFound`]: no live output channel exists for this id
    ///   (bundled `SupervisorApiAdapter`).
    ///
    async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError>;
}
