//! # Runner trait.
//!
//! [`Runner`] is the plugin interface for task executors.
//! Concrete runners implement this trait and are registered in the [`RunnerRouter`](crate::RunnerRouter).
//!
//! See the [crate root](crate) for architecture overview and routing rules.

use solti_model::TaskSpec;
use taskvisor::TaskRef;

use crate::context::BuildContext;
use crate::error::RunnerError;
use crate::id::{RunId, make_run_id};

/// Generic task runner used by the core layer.
///
/// A runner is responsible for:
/// - deciding whether it can handle a given [`TaskSpec`] (`supports`)
/// - building a concrete [`TaskRef`] that the supervisor can execute (`build_task`)
///
/// ## Also
///
/// - [`RunnerRouter`](crate::RunnerRouter) selects a runner for a given spec.
/// - [`RunId`](crate::RunId) is a default id format produced by [`build_run_id`](Self::build_run_id).
/// - [`BuildContext`](crate::BuildContext) shared dependencies passed to [`build_task`](Self::build_task).
pub trait Runner: Send + Sync {
    /// Runner name used in logs and diagnostics.
    fn name(&self) -> &'static str;

    /// Returns `true` if this runner can handle the given spec.
    fn supports(&self, spec: &TaskSpec) -> bool;

    /// Build a concrete [`TaskRef`] for the given spec.
    ///
    /// The provided [`BuildContext`] carries shared dependencies injected at router setup time.
    /// Slot is extracted from `spec.slot`.
    fn build_task(&self, spec: &TaskSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError>;

    /// Builds a default run id for a given slot.
    ///
    /// Runners may override this if they need custom id format, otherwise the core helper is used.
    fn build_run_id(&self, slot: &str) -> RunId {
        make_run_id(self.name(), slot)
    }
}
