//! # Container runner
//!
//! [`ContainerRunner`] converts `Container` resources into Taskvisor tasks.
//! A [`ContainerEngine`] owns engine-specific image and process operations.
//!
//! Building a task performs no engine I/O.
//! Each execution attempt receives a unique identifier and owns its resources.

mod config;
pub use config::ContainerRunnerConfig;

mod engine;
pub use engine::{
    ContainerAttempt, ContainerEngine, ContainerEngineError, ContainerEngineInfo,
    ContainerErrorClass, ContainerExitStatus, ContainerOutput, ContainerRequest,
};

#[cfg(feature = "containerd")]
mod oci;

mod policy;
pub use policy::ContainerProcessPolicy;

mod runner;
pub use runner::ContainerRunner;

#[cfg(feature = "containerd")]
#[cfg_attr(docsrs, doc(cfg(feature = "containerd")))]
pub mod containerd;

use std::sync::Arc;

use solti_model::Labels;
use solti_runner::{Runner, RunnerRouter};

pub use crate::output::LogConfig;
pub use crate::registration::LABEL_RUNNER_NAME;

/// Registers a container runner with default settings.
///
/// The runner receives label `solti.io/runner-name=<name>`.
///
/// # Errors
///
/// Returns [`crate::ExecError`] when configuration or registration fails.
pub fn register_container_runner(
    router: &mut RunnerRouter,
    name: impl Into<String>,
    engine: Arc<dyn ContainerEngine>,
) -> Result<(), crate::ExecError> {
    register_runner(router, Arc::new(ContainerRunner::new(name, engine)?))
}

/// Registers a container runner with explicit settings.
///
/// The runner receives label `solti.io/runner-name=<name>`.
///
/// # Errors
///
/// Returns [`crate::ExecError`] when configuration or registration fails.
pub fn register_container_runner_with_config(
    router: &mut RunnerRouter,
    name: impl Into<String>,
    engine: Arc<dyn ContainerEngine>,
    config: ContainerRunnerConfig,
) -> Result<(), crate::ExecError> {
    register_runner(
        router,
        Arc::new(ContainerRunner::with_config(name, engine, config)?),
    )
}

fn register_runner(
    router: &mut RunnerRouter,
    runner: Arc<ContainerRunner>,
) -> Result<(), crate::ExecError> {
    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_NAME, runner.name());
    router.register_with_labels(runner, labels)?;
    Ok(())
}
