//! # Runner trait.
//!
//! [`Runner`] is the plugin interface for task executors.
//! Concrete runners implement this trait and are registered in a [`RunnerRouter`](crate::RunnerRouter).
//!
//! A runner does not supervise the task.
//! It only builds a `taskvisor::TaskRef` from a Solti task spec.

use solti_model::TaskSpec;
use taskvisor::TaskRef;

use crate::context::BuildContext;
use crate::error::RunnerError;
use crate::id::{RunId, make_run_id};

/// Plugin trait for task execution backends.
///
/// A runner is responsible for:
/// - saying whether it can handle a [`TaskSpec`];
/// - building a concrete [`TaskRef`] that `taskvisor` can run.
///
/// ## Example
///
/// ```rust,no_run
/// use solti_model::{TaskKind, TaskSpec};
/// use solti_runner::{BuildContext, Runner, RunnerError};
/// use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
///
/// struct MyRunner;
///
/// impl Runner for MyRunner {
///     fn name(&self) -> &'static str {
///         "my-runner"
///     }
///
///     fn supports(&self, spec: &TaskSpec) -> bool {
///         matches!(spec.kind(), TaskKind::Subprocess(_))
///     }
///
///     fn build_task(&self, spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
///         let run_id = self.build_run_id(spec.slot().as_ref());
///         Ok(TaskFn::arc(run_id.into_name(), |_ctx: TaskContext| async move {
///             Ok::<(), TaskError>(())
///         }))
///     }
/// }
/// ```
///
/// ## Also
///
/// - [`RunnerRouter`](crate::RunnerRouter) selects a runner for a given spec.
/// - [`RunId`](crate::RunId) is a default id format produced by [`build_run_id`](Self::build_run_id).
/// - [`BuildContext`](crate::BuildContext) shared dependencies passed to [`build_task`](Self::build_task).
pub trait Runner: Send + Sync {
    /// Return the runner name used in logs, metrics, and run ids.
    fn name(&self) -> &'static str;

    /// Returns `true` if this runner can handle the given spec.
    fn supports(&self, spec: &TaskSpec) -> bool;

    /// Build a concrete [`TaskRef`] for the given spec.
    ///
    /// The [`BuildContext`] carries shared dependencies injected at router setup time, such as env, metrics, and the output producer capability.
    /// Build may be followed by a rejected submission, and the returned task may
    /// run more than once. Capture immutable configuration here; acquire
    /// attempt-scoped resources, including output sinks, inside the task body.
    fn build_task(&self, spec: &TaskSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError>;

    /// Builds a default run id for a given slot.
    ///
    /// Runners may override this if they need a custom id format.
    ///
    /// ## Example
    ///
    /// ```
    /// # use solti_model::TaskSpec;
    /// # use solti_runner::{BuildContext, Runner, RunnerError};
    /// # use taskvisor::TaskRef;
    /// struct MyRunner;
    ///
    /// impl Runner for MyRunner {
    ///     fn name(&self) -> &'static str { "my-runner" }
    ///     fn supports(&self, _spec: &TaskSpec) -> bool { true }
    ///     fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
    ///         unimplemented!()
    ///     }
    /// }
    ///
    /// let id = MyRunner.build_run_id("slot-a");
    /// assert!(id.name().starts_with("my-runner-slot-a-"));
    /// ```
    fn build_run_id(&self, slot: &str) -> RunId {
        make_run_id(self.name(), slot)
    }
}
