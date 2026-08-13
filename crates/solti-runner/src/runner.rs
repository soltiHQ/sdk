//! # Runner trait
//!
//! [`Runner`] is the plugin boundary for execution backends.
//! Implementations are registered in [`RunnerRouter`](crate::RunnerRouter).
//!
//! ## Flow
//!
//! ```text
//! Task + RunId + BuildContext + BuildCancellation + BuildScope
//!                              ▼
//!                            Runner
//!                              ▼
//!                       taskvisor::TaskRef
//! ```
//!
//! The runner builds the task.
//! Taskvisor supervises its execution.

use solti_model::{Task, WorkloadTypeMeta};
use taskvisor::TaskRef;

use async_trait::async_trait;

use crate::BuildScope;
use crate::cancellation::BuildCancellation;
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
/// - Building is asynchronous and must remain owned by the returned future.
/// - Implementations must observe [`BuildCancellation`] during interruptible work.
/// - Composing runners must pass [`BuildScope`] to scoped catalog builds.
/// - The returned task must not retain the build cancellation signal.
/// - Attempt-scoped resources belong inside the task body.
///
/// A returned task may execute more than one attempt.
/// A supervisor drops the build future after cancellation or its configured
/// deadline. Implementations must not detach work from that future. Inherently
/// blocking work belongs in a runner-owned bounded facility with an explicit
/// cancellation and shutdown contract.
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
/// #[solti_runner::async_trait]
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
///     async fn build_task(
///         &self,
///         _task: &Task,
///         run_id: &solti_runner::RunId,
///         _ctx: &BuildContext,
///         _cancellation: &solti_runner::BuildCancellation,
///         _scope: &mut solti_runner::BuildScope,
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
#[async_trait]
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
    /// [`BuildScope`] carries admission through nested catalog builds.
    /// The returned task name must equal `run_id.name()`.
    ///
    /// # Errors
    ///
    /// Returns [`RunnerError`] when the workload cannot be converted.
    async fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
        cancellation: &BuildCancellation,
        scope: &mut BuildScope,
    ) -> Result<TaskRef, RunnerError>;
}
