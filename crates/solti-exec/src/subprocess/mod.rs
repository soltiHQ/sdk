//! # Subprocess Runner
//!
//! [`SubprocessRunner`] executes `TaskWorkload::Subprocess` resources.
//! Each attempt gets its own process group, output streams, and optional cgroup.
//!
//! ## Registration
//!
//! ```text
//! register_subprocess_runner(&mut router, "my-runner")
//!     ├──► creates SubprocessRunner::new("my-runner")
//!     ├──► attaches label "solti.io/runner-name" = "my-runner"
//!     └──► registers in RunnerRouter
//!
//! register_subprocess_runner_with_backend(&mut router, "secure", backend)
//!     ├──► validates SubprocessBackendConfig
//!     ├──► creates SubprocessRunner::with_config("secure", backend)
//!     ├──► attaches label "solti.io/runner-name" = "secure"
//!     └──► registers in RunnerRouter
//! ```
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

/// Well-known label key used to identify a runner by name.
pub const LABEL_RUNNER_NAME: &str = "solti.io/runner-name";

/// Register a subprocess runner with default settings.
///
/// Creates a [`SubprocessRunner`] with default settings, labels it with
/// [`LABEL_RUNNER_NAME`]` = name`, and adds it to the router.
///
/// ## Errors
///
/// - [`ExecError::InvalidRunnerConfig`]: `name` is not a Kubernetes label value.
/// - [`ExecError::Router`]: the router rejects the registration.
pub fn register_subprocess_runner(
    router: &mut RunnerRouter,
    name: impl Into<String>,
) -> Result<(), ExecError> {
    register_runner_inner(router, Arc::new(SubprocessRunner::new(name)?))
}

/// Register a subprocess runner with explicit runner configuration.
///
/// Validates `backend` first, then registers the runner under
/// [`LABEL_RUNNER_NAME`]` = name`.
///
/// ## Errors
///
/// - [`ExecError::InvalidRunnerConfig`]: `name` or `backend` is invalid.
/// - [`ExecError::Router`]: the router rejects the registration.
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
