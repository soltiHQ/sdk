//! # Subprocess runner
//!
//! [`SubprocessRunner`] executes [`TaskWorkload::Subprocess`](solti_model::TaskWorkload::Subprocess) resources.
//! It supports command and script modes.
//!
//! ## Flow
//!
//! ```text
//! Task ──► SubprocessRunner ──► TaskRef
//!                                  │ each attempt
//!                                  ▼
//!                          prepare backend
//!                                  ▼
//!                          spawn process
//!                           ├──► stdout/stderr
//!                           └──► exit/cancel
//! ```
//!
//! Each attempt owns its process, output readers, script file, and optional cgroup.
//! Unix attempts use a dedicated process group.
//! The runner removes attempt-scoped resources after completion.
//!
//! ## Registration
//!
//! [`register_subprocess_runner`] uses default backend settings.
//! [`register_subprocess_runner_with_backend`] accepts explicit settings.
//! Both helpers add [`LABEL_RUNNER_NAME`] to the registered runner labels.
mod backend;
pub use backend::{CwdPolicy, EnvPolicy, SubprocessBackendConfig};

mod task;

mod logger;
pub use logger::LogConfig;

mod runner;
pub use runner::SubprocessRunner;

use std::sync::Arc;

use solti_model::Labels;
use solti_runner::{Runner, RunnerRouter};

use crate::ExecError;

/// Runner label used by `runnerSelector`.
pub const LABEL_RUNNER_NAME: &str = "solti.io/runner-name";

/// Registers a subprocess runner with default settings.
///
/// The runner receives label `solti.io/runner-name=<name>`.
///
/// # Errors
///
/// Returns [`ExecError::InvalidRunnerConfig`] when `name` is invalid.
/// Returns [`ExecError::Router`] when the router rejects registration.
pub fn register_subprocess_runner(
    router: &mut RunnerRouter,
    name: impl Into<String>,
) -> Result<(), ExecError> {
    register_runner_inner(router, Arc::new(SubprocessRunner::new(name)?))
}

/// Registers a subprocess runner with explicit backend settings.
///
/// The backend is validated before registration.
/// The runner receives label `solti.io/runner-name=<name>`.
///
/// # Errors
///
/// Returns [`ExecError::InvalidRunnerConfig`] when `name` or `backend` is invalid.
/// Returns [`ExecError::Io`] when current cgroup discovery fails.
/// Returns [`ExecError::Router`] when the router rejects registration.
pub fn register_subprocess_runner_with_backend(
    router: &mut RunnerRouter,
    name: impl Into<String>,
    backend: SubprocessBackendConfig,
) -> Result<(), ExecError> {
    register_runner_inner(
        router,
        Arc::new(SubprocessRunner::with_config(name, backend)?),
    )
}

fn register_runner_inner(
    router: &mut RunnerRouter,
    runner: Arc<SubprocessRunner>,
) -> Result<(), ExecError> {
    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_NAME, runner.name());
    router.register_with_labels(runner, labels)?;
    Ok(())
}
