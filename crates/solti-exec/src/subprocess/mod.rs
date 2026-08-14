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
//!                        reserve cleanup ownership
//!                                  ▼
//!                          prepare backend
//!                                  ▼
//!                          spawn process
//!                           ├──► stdout/stderr
//!                           └──► exit/cancel
//! ```
//!
//! Each attempt owns its process and output readers.
//! Script attempts also own an anonymous script descriptor.
//! A configured cgroup is attempt-scoped.
//! Unix attempts use a dedicated process group.
//! The runner removes attempt-scoped resources after completion.
//! Registration returns the concrete runner used for finalizer status and shutdown.
//!
//! ## Registration
//!
//! [`register_subprocess_runner`] uses default backend settings.
//! [`register_subprocess_runner_with_backend`] accepts explicit settings.
//! Both helpers add [`LABEL_RUNNER_NAME`] to the registered runner labels.
mod backend;
pub use backend::{
    CwdPolicy, DEFAULT_SUBPROCESS_CLEANUP_CAPACITY, EnvPolicy, SubprocessBackendConfig,
};

mod boundary;

mod child;

mod domain;
pub use domain::SubprocessFinalizerStatus;

#[cfg(target_os = "macos")]
mod spawn_macos;

mod task;

pub use crate::output::LogConfig;

mod script;

mod runner;
pub use runner::SubprocessRunner;

use std::sync::Arc;

use solti_model::Labels;
use solti_runner::{Runner, RunnerRouter};

use crate::ExecError;
pub use crate::registration::LABEL_RUNNER_NAME;

/// Registers a subprocess runner with default settings.
///
/// The default uses an empty [`crate::host::HostProcessPolicy`].
/// The runner receives label `solti.io/runner-name=<name>`.
/// The returned handle exposes finalizer status and terminal shutdown.
///
/// # Errors
///
/// Returns [`ExecError::InvalidRunnerConfig`] when `name` is invalid.
/// Returns [`ExecError::Router`] when the router rejects registration.
/// Returns [`ExecError::Io`] when the cleanup worker cannot start.
pub fn register_subprocess_runner(
    router: &mut RunnerRouter,
    name: impl Into<String>,
) -> Result<Arc<SubprocessRunner>, ExecError> {
    register_runner_inner(router, Arc::new(SubprocessRunner::new(name)?))
}

/// Registers a subprocess runner with explicit backend settings.
///
/// The backend is validated before registration.
/// The runner receives label `solti.io/runner-name=<name>`.
/// The returned handle exposes finalizer status and terminal shutdown.
///
/// # Errors
///
/// Returns [`ExecError::InvalidRunnerConfig`] when `name` or `backend` is invalid.
/// Returns [`ExecError::Router`] when the router rejects registration.
/// Returns [`ExecError::Io`] when backend preparation fails or the cleanup worker
/// cannot start.
pub fn register_subprocess_runner_with_backend(
    router: &mut RunnerRouter,
    name: impl Into<String>,
    backend: SubprocessBackendConfig,
) -> Result<Arc<SubprocessRunner>, ExecError> {
    register_runner_inner(
        router,
        Arc::new(SubprocessRunner::with_config(name, backend)?),
    )
}

fn register_runner_inner(
    router: &mut RunnerRouter,
    runner: Arc<SubprocessRunner>,
) -> Result<Arc<SubprocessRunner>, ExecError> {
    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_NAME, runner.name());
    router.register_with_labels(runner.clone(), labels)?;
    Ok(runner)
}
