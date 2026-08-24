//! # Handler Boundary
//!
//! [`ApiHandler`] is the shared backend for HTTP and gRPC.
//! It receives validated [`solti_model`] values.
//! It returns domain values or [`ApiError`].
//!
//! ```text
//! HTTP handlers ──┐
//!                 ├──► ApiHandler ──► backend
//! gRPC service ───┘
//! ```
//!
//! Wire encoding stays outside the handler.
//! A custom implementation can use another store or wrap another backend.

use std::pin::Pin;

use async_trait::async_trait;
use solti_model::{
    OutputEvent, Task, TaskFilter, TaskId, TaskManifest, TaskPage, TaskQuery, TaskRunPage,
    TaskRunQuery, TaskWatchEvent, WritePreconditions,
};
use tokio_stream::Stream;

use crate::error::ApiError;

/// Boxed live stream of task output events.
///
/// The stream item is [`OutputEvent`].
/// Transport adapters encode each item for their wire format.
pub type OutputEventStream = Pin<Box<dyn Stream<Item = OutputEvent> + Send + 'static>>;

/// Boxed stream of task resource changes.
///
/// A stream item can contain a terminal [`ApiError`].
pub type TaskWatchEventStream =
    Pin<Box<dyn Stream<Item = Result<TaskWatchEvent, ApiError>> + Send + 'static>>;

/// Transport-independent task API.
///
/// The trait covers desired writes, current reads, collection watches,
/// run history, cancellation, deletion, and live output.
///
/// Implementations must not expose the built-in `Embedded` workload.
/// Both transports check that boundary before encoding a response.
///
/// ## Operations
///
/// | Method             | HTTP                                         | gRPC             |
/// |--------------------|----------------------------------------------|------------------|
/// | `create_task`      | `POST   /apis/solti.io/v1/tasks`             | `CreateTask`     |
/// | `apply_task`       | `PUT    /apis/solti.io/v1/tasks/{name}`      | `ApplyTask`      |
/// | `get_task`         | `GET    /apis/solti.io/v1/tasks/{name}`      | `GetTask`        |
/// | `query_tasks`      | `GET    /apis/solti.io/v1/tasks`             | `ListTasks`      |
/// | `watch_tasks`      | `GET    /apis/solti.io/v1/tasks?watch=true`  | `WatchTasks`     |
/// | `query_task_runs`  | `GET    /apis/solti.io/v1/tasks/{name}/runs` | `ListTaskRuns`   |
/// | `cancel_task`      | `POST   /apis/solti.io/v1/tasks/{name}/cancel` | `CancelTask`    |
/// | `delete_task`      | `DELETE /apis/solti.io/v1/tasks/{name}`      | `DeleteTask`     |
/// | `stream_task_logs` | `GET    /apis/solti.io/v1/tasks/{name}/logs` | `StreamTaskLogs` |
///
/// ## See Also
///
/// - `SupervisorApiAdapter` implements this trait for `solti-core`.
/// - [`ApiError`] defines the shared transport error categories.
#[async_trait]
pub trait ApiHandler: Send + Sync + 'static {
    /// Creates one named task resource.
    ///
    /// The bundled adapter returns committed desired state immediately.
    /// Reconciliation continues in the background.
    /// Its result appears in `status.conditions[type=Reconciled]`.
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns:
    ///
    /// - [`ApiError::InvalidRequest`] when the manifest is rejected.
    /// - [`ApiError::AlreadyExists`] when the name is retained.
    /// - [`ApiError::ResourceExhausted`] when a retained Task budget rejects the write.
    /// - [`ApiError::Unavailable`] after shutdown starts.
    ///
    /// Later reconciliation failures are status updates.
    /// They are not create errors.
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError>;

    /// Creates or updates the task addressed by `metadata.name`.
    ///
    /// Empty preconditions make this an upsert.
    /// Any precondition requires an existing matching resource.
    ///
    /// ## Errors
    ///
    /// The bundled adapter can return the errors from
    /// [`create_task`](Self::create_task).
    /// It can also return:
    ///
    /// - [`ApiError::TaskNotFound`] when conditional apply finds no task.
    /// - [`ApiError::Conflict`] when a precondition does not match.
    /// - [`ApiError::ResourceExhausted`] when positive TaskManifest growth
    ///   would exceed the retained byte budget.
    async fn apply_task(
        &self,
        manifest: TaskManifest,
        preconditions: WritePreconditions,
    ) -> Result<Task, ApiError>;

    /// Returns the current task resource with this name.
    ///
    /// `None` means that no public task has this name.
    ///
    /// ## Errors
    ///
    /// The bundled adapter does not return an error.
    /// A custom implementation can return any [`ApiError`].
    async fn get_task(&self, name: &TaskId) -> Result<Option<Task>, ApiError>;

    /// Returns one filtered task page.
    ///
    /// The returned page must match the query filters and count limit.
    /// Implementations should also honor the query item byte limit before cloning items.
    /// A continuation page must keep the requested snapshot and must not repeat
    /// the Task named by the requested cursor. Its returned continuation must
    /// describe the same snapshot and filter.
    /// The transports reject an inconsistent page as [`ApiError::Internal`].
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns:
    ///
    /// - [`ApiError::InvalidRequest`] for an invalid continuation.
    /// - [`ApiError::ResourceVersionExpired`] for a compacted snapshot.
    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError>;

    /// Watches changes to tasks that match the filter.
    ///
    /// With the bundled adapter, an absent resource version or `"0"` first
    /// emits current matches as `Added`.
    /// A specific version replays newer retained changes.
    /// Both forms then continue with live changes.
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns:
    ///
    /// - [`ApiError::InvalidRequest`] for an invalid resource version.
    /// - [`ApiError::ResourceVersionExpired`] when the requested position is
    ///   no longer retained.
    /// - [`ApiError::ResourceExhausted`] when concurrent-watch or retained
    ///   initial/replay admission is full.
    ///
    /// The stream can later yield the same error when it falls behind.
    /// That error is terminal.
    async fn watch_tasks(
        &self,
        filter: TaskFilter,
        resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError>;

    /// Returns one snapshot-consistent page of a task's execution attempts.
    ///
    /// Runs are ordered from oldest to newest.
    /// The returned page must match the requested task, count limit, and continuation snapshot.
    /// Implementations should honor the query item byte limit before cloning runs.
    /// The transports reject an inconsistent page as [`ApiError::Internal`].
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns:
    ///
    /// - [`ApiError::TaskNotFound`] when a first-page task is not public or does not exist.
    /// - [`ApiError::InvalidRequest`] for an invalid continuation.
    /// - [`ApiError::ResourceVersionExpired`] for a compacted run snapshot.
    async fn query_task_runs(
        &self,
        id: &TaskId,
        query: TaskRunQuery,
    ) -> Result<TaskRunPage, ApiError>;

    /// Requests a terminal logical outcome while retaining desired state and run history.
    /// Cancellation does not suppress later reconciliation.
    /// Force-aborted task code can remain physically active after this call returns.
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns:
    ///
    /// - [`ApiError::TaskNotFound`] when the task is not public or does not exist.
    /// - [`ApiError::Conflict`] when a precondition does not match.
    /// - [`ApiError::Unavailable`] after supervisor shutdown closes operation admission.
    /// - [`ApiError::Internal`] when runtime cancellation fails.
    async fn cancel_task(
        &self,
        id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError>;

    /// Requests a terminal logical outcome and removes one task and its run history.
    /// Force-aborted task code can remain physically active after this call returns.
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns:
    ///
    /// - [`ApiError::TaskNotFound`] when the task is not public or does not exist.
    /// - [`ApiError::Conflict`] when a precondition does not match.
    /// - [`ApiError::Internal`] when runtime cancellation fails.
    async fn delete_task(
        &self,
        id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError>;

    /// Subscribes to one task's live output.
    ///
    /// The stream is lossy and has no replay.
    /// It can cover later attempts of the same task generation.
    /// Run boundary events are best-effort observations.
    /// They are not ordering barriers for output chunks.
    ///
    /// The bundled adapter pins the stream to the generation visible
    /// when this method is called.
    ///
    /// ## Errors
    ///
    /// The bundled adapter returns [`ApiError::TaskNotFound`]
    /// when no public live output channel exists for this task.
    async fn stream_task_logs(&self, id: &TaskId) -> Result<OutputEventStream, ApiError>;
}
