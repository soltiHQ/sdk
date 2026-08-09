//! # Runner trait
//!
//! [`Runner`] is the plugin boundary for execution backends.
//! Implementations are registered in [`RunnerRouter`](crate::RunnerRouter).
//!
//! ## Flow
//!
//! ```text
//! Task + RunId + BuildContext
//!              ▼
//!            Runner
//!              ▼
//!       taskvisor::TaskRef
//! ```
//!
//! The runner builds the task.
//! Taskvisor supervises its execution.

use solti_model::{Task, WorkloadTypeMeta};
use taskvisor::TaskRef;

use crate::context::BuildContext;
use crate::error::RunnerError;
use crate::id::RunId;

/// Plugin trait for task execution backends.
///
/// A runner declares a finite set of workload GVKs.
/// It converts matching task resources into [`TaskRef`] values.
///
/// ## Contract
///
/// - [`build_task`](Self::build_task) must use the allocated [`RunId`] as the task name.
/// - The router snapshots the name and workload GVKs during registration.
/// - Building must not start or submit the task.
/// - Building is synchronous and must finish without unbounded waits.
/// - Attempt-scoped resources belong inside the task body.
///
/// A returned task may execute more than one attempt.
/// A supervisor may stop awaiting a build during shutdown. Rust cannot forcibly
/// stop synchronous code that is already running, so implementations must bound
/// their own blocking work and must not rely on the build result being observed.
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
/// ## See Also
///
/// - [`RunnerRouter`](crate::RunnerRouter)
/// - [`BuildContext`](crate::BuildContext)
/// - [`RunId`](crate::RunId)
pub trait Runner: Send + Sync {
    /// Returns the runner name.
    ///
    /// The router validates and snapshots it during registration.
    /// The snapshot is used for capabilities and run ids.
    fn name(&self) -> &str;

    /// Returns the workload GVKs handled by this runner.
    ///
    /// The router snapshots and validates this declaration during registration.
    /// Routing and capability introspection use the same snapshot.
    fn workload_types(&self) -> Vec<WorkloadTypeMeta>;

    /// Builds a [`TaskRef`] for a matching task resource.
    ///
    /// [`BuildContext`] provides environment, metrics, and output publishing.
    /// The returned task name must equal `run_id.name()`.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when the workload cannot be converted.
    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError>;
}
