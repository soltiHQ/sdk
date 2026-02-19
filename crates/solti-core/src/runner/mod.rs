//! Runner abstraction used by `solti-core` to build taskvisor tasks from `CreateSpec`.
//!
//! Concrete runners implement this trait and are plugged into the router.
mod error;
pub use error::RunnerError;

mod context;
pub use context::BuildContext;

mod id;
pub use id::make_run_id;

use solti_model::CreateSpec;
use taskvisor::TaskRef;

/// Generic task runner used by the core layer.
///
/// A runner is responsible for:
/// - deciding whether it can handle a given [`CreateSpec`] (`supports`)
/// - building a concrete [`TaskRef`] that the supervisor can execute (`build_task`)
pub trait Runner: Send + Sync {
    /// Runner name used in logs and diagnostics.
    fn name(&self) -> &'static str;

    /// Returns `true` if this runner can handle the given spec.
    fn supports(&self, spec: &CreateSpec) -> bool;

    /// Build a concrete [`TaskRef`] for the given spec.
    ///
    /// The provided [`BuildContext`] carries shared dependencies injected at router setup time.
    fn build_task(&self, spec: &CreateSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError>;

    /// Builds a default run id for a given slot.
    ///
    /// Runners may override this if they need custom id format,
    /// otherwise the core helper is used.
    fn build_run_id(&self, slot: &str) -> String {
        make_run_id(self.name(), slot)
    }
}
