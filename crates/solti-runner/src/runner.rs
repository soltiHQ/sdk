//! # Runner trait
//!
//! [`Runner`] is the plugin interface for task executors.
//! Concrete runners implement this trait and are registered in a [`RunnerRouter`](crate::RunnerRouter).
//!
//! A runner does not supervise the task.
//! It only builds a `taskvisor::TaskRef` from a Solti [`Task`].

use solti_model::{Task, WorkloadTypeMeta};
use taskvisor::TaskRef;

use crate::context::BuildContext;
use crate::error::RunnerError;
use crate::id::RunId;

/// Plugin trait for task execution backends.
///
/// A runner is responsible for:
/// - declaring the workload GVKs it can handle;
/// - building a concrete [`TaskRef`] that `taskvisor` can run.
///
/// ## Example
///
/// ```rust,no_run
/// use solti_model::{Task, WorkloadTypeMeta, WORKLOAD_API_VERSION};
/// use solti_runner::{BuildContext, Runner, RunnerError};
/// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
///
/// struct MyRunner;
///
/// impl Runner for MyRunner {
///     fn name(&self) -> &str {
///         "my-runner"
///     }
///
///     fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
///         vec![
///             WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess")
///                 .expect("built-in workload GVK"),
///         ]
///     }
///
///     fn build_task(
///         &self,
///         _task: &Task,
///         run_id: &solti_runner::RunId,
///         _ctx: &BuildContext,
///     ) -> Result<TaskRef, RunnerError> {
///         Ok(TaskFn::arc(run_id.name(), |_ctx: TaskContext| async move {
///             Ok::<(), TaskError>(())
///         }))
///     }
/// }
/// ```
///
/// ## Also
///
/// - [`RunnerRouter`](crate::RunnerRouter) selects a runner for a given task workload.
/// - [`RunId`](crate::RunId) identifies the taskvisor runtime allocated by the router.
/// - [`BuildContext`](crate::BuildContext) shared dependencies passed to [`build_task`](Self::build_task).
pub trait Runner: Send + Sync {
    /// Return the runner name used in logs, metrics, and run ids.
    ///
    /// The name must remain stable for the lifetime of the runner.
    fn name(&self) -> &str;

    /// Return the finite set of workload GVKs handled by this runner.
    ///
    /// The router snapshots and validates this declaration during registration.
    /// Routing and capability introspection use that same immutable snapshot.
    /// Validate workload-specific desired state in
    /// [`build_task`](Self::build_task).
    fn workload_types(&self) -> Vec<WorkloadTypeMeta>;

    /// Build a concrete [`TaskRef`] for the given task resource.
    ///
    /// [`BuildContext`] provides shared env, metrics, and output publishing.
    /// Build may be followed by a rejected submission, and the returned task may
    /// run more than once. Capture immutable configuration here; acquire
    /// attempt-scoped resources, including output sinks, inside the task body.
    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError>;
}
