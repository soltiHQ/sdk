//! Subprocess runner for `tno_model::TaskKind::Exec`.
//!
//! Translates `TaskKind::Exec` specs into `TaskRef` instances that
//! spawn child processes via `tokio::process::Command`.
mod runner;
mod config;

pub use runner::SubprocessRunner;

use std::sync::Arc;
use tno_core::RunnerRouter;

/// Register the built-in subprocess runner in the given router.
///
/// After this call, any `CreateSpec` with `TaskKind::Exec { .. }` will be handled by [`SubprocessRunner`].
pub fn register_subprocess_runner(router: &mut RunnerRouter) {
    router.register(Arc::new(SubprocessRunner::new()));
}