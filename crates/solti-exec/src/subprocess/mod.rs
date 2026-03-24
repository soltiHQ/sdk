//! # Subprocess: OS process runner for `TaskKind::Subprocess`.
//!
//! Executes tasks by spawning child OS processes with optional backend hardening (rlimits, cgroups, security capabilities).
//!
//! ## Modules
//!
//! | Module      | What it does                                            |
//! |-------------|---------------------------------------------------------|
//! | [`runner`]  | [`SubprocessRunner`]: `Runner` trait impl + execution   |
//! | [`backend`] | [`SubprocessBackendConfig`]: rlimits, cgroups, security |
//! | [`task`]    | [`SubprocessTaskConfig`]: resolved runtime config       |
//! | [`logger`]  | [`LogConfig`] + stream capture, truncation, tracing     |
//!
//! ## Quick start
//! ```text
//! register_subprocess_runner(&mut router, "my-runner")
//!     ├──► creates SubprocessRunner::new("my-runner")
//!     ├──► attaches label "runner-name" = "my-runner"
//!     └──► registers in RunnerRouter
//!
//! register_subprocess_runner_with_backend(&mut router, "secure", backend)
//!     ├──► validates SubprocessBackendConfig
//!     ├──► creates SubprocessRunner::with_config("secure", backend)
//!     ├──► attaches label "runner-name" = "secure"
//!     └──► registers in RunnerRouter
//! ```
//!
//! ## Registration guard
//! - Duplicate runner names are rejected via `router.contains_label()` check
//! - Returns `ExecError::DuplicateRunner` if a runner with the same name exists
mod backend;
pub use backend::SubprocessBackendConfig;

mod task;
pub use task::SubprocessTaskConfig;

mod logger;
pub use logger::LogConfig;

mod runner;
pub use runner::SubprocessRunner;

use std::sync::Arc;

use solti_core::RunnerRouter;
use solti_model::Labels;

use crate::ExecError;

/// Well-known label key used to identify a runner by name.
pub const LABEL_RUNNER_NAME: &str = "runner-name";

/// Register a subprocess runner with default settings.
pub fn register_subprocess_runner(
    router: &mut RunnerRouter,
    name: &'static str,
) -> Result<(), ExecError> {
    register_runner_inner(router, name, Arc::new(SubprocessRunner::new(name)))
}

/// Register a subprocess runner with explicit runner configuration.
pub fn register_subprocess_runner_with_backend(
    router: &mut RunnerRouter,
    name: &'static str,
    backend: SubprocessBackendConfig,
) -> Result<(), ExecError> {
    register_runner_inner(
        router,
        name,
        Arc::new(SubprocessRunner::with_config(name, backend)?),
    )
}

fn register_runner_inner(
    router: &mut RunnerRouter,
    name: &'static str,
    runner: Arc<SubprocessRunner>,
) -> Result<(), ExecError> {
    if router.contains_label(LABEL_RUNNER_NAME, name) {
        return Err(ExecError::DuplicateRunner {
            name: name.to_string(),
        });
    }
    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_NAME, name);
    router.register_with_labels(runner, labels);
    Ok(())
}
