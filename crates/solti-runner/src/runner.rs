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

use solti_model::{Task, WorkloadDecl};
use taskvisor::TaskRef;

use async_trait::async_trait;

use crate::BuildScope;
use crate::cancellation::BuildCancellation;
use crate::context::BuildContext;
use crate::error::RunnerError;
use crate::id::RunId;

/// Plugin trait for task execution backends.
///
/// A runner declares a finite set of workload types: a GVK, the JSON Schema of its `spec` and a body check.
/// It converts matching task resources into [`TaskRef`] values.
///
/// ## Contract
///
/// - The router returns the allocated [`RunId`] beside the task in a [`BuiltTask`](crate::BuiltTask).
/// - Implementations must observe [`BuildCancellation`] during interruptible work.
/// - Building is asynchronous and must remain owned by the returned future.
/// - Composing runners must pass [`BuildScope`] to scoped catalog builds.
/// - The router snapshots the name and workload declarations during registration.
/// - The returned task must not retain the build cancellation signal.
/// - Attempt-scoped resources belong inside the task body.
/// - Building must not start or submit the task.
///
/// A returned task may execute more than one attempt.
/// A supervisor drops the build future after cancellation or its configured deadline.
/// Implementations must not detach work from that future.
/// Inherently blocking work belongs in a runner-owned bounded facility with an explicit cancellation and shutdown contract.
///
/// ## Example
///
/// ```rust,no_run
/// use serde::Deserialize;
/// use schemars::JsonSchema;
/// use solti_model::{Task, WorkloadBody, WorkloadDecl};
/// use solti_runner::{BuildContext, Runner, RunnerError};
/// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
///
/// #[derive(Deserialize, JsonSchema)]
/// #[serde(deny_unknown_fields)]
/// struct Echo {
///     message: String,
/// }
///
/// impl WorkloadBody for Echo {
///     const API_VERSION: &'static str = "tasks.example.io/v1";
///     const KIND: &'static str = "Echo";
/// }
///
/// struct MyRunner;
///
/// #[solti_runner::async_trait]
/// impl Runner for MyRunner {
///     fn name(&self) -> &str {
///         "my-runner"
///     }
///
///     fn workload_types(&self) -> Vec<WorkloadDecl> {
///         vec![WorkloadDecl::of::<Echo>()]
///     }
///
///     async fn build_task(
///         &self,
///         _task: &Task,
///         _run_id: &solti_runner::RunId,
///         _ctx: &BuildContext,
///         _cancellation: &solti_runner::BuildCancellation,
///         _scope: &mut solti_runner::BuildScope,
///     ) -> Result<TaskRef, RunnerError> {
///         Ok(TaskFn::arc(|_ctx: TaskContext| async move {
///             Ok::<(), TaskError>(())
///         }))
///     }
/// }
/// ```
///
/// ## See Also
///
/// - [`RunnerRouter`](crate::RunnerRouter)
/// - [`BuildContext`]
/// - [`RunId`]
#[async_trait]
pub trait Runner: Send + Sync {
    /// Returns the runner name.
    ///
    /// The router validates and snapshots it during registration.
    /// The snapshot is used for capabilities and run ids.
    fn name(&self) -> &str;

    /// Returns the workload types handled by this runner.
    ///
    /// The router reads this once during registration, refuses an unusable declaration and snapshots the rest.
    /// Routing, admission checks and capability introspection use the same snapshot.
    /// Declaration order is not routing order; registration order of the runners is.
    ///
    /// Each declaration's check runs under the task's write lock and a state-write admission
    /// permit, so it must be fast and non-blocking.
    fn workload_types(&self) -> Vec<WorkloadDecl>;

    /// Builds a [`TaskRef`] for a matching task resource.
    ///
    /// [`BuildContext`] provides environment, metrics, and output publishing.
    /// [`BuildScope`] carries admission through nested catalog builds.
    /// The router returns `run_id` with the executable task in a [`BuiltTask`](crate::BuiltTask).
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
