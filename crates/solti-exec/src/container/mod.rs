//! # Container runner
//!
//! [`ContainerRunner`] converts `Container` resources into Taskvisor tasks.
//! A [`ContainerEngine`] owns engine-specific image and process operations.
//!
//! Building a task performs no engine I/O.
//! Each execution attempt receives a unique identifier and owns its resources.
//!
//! Custom engines require an explicit [`ContainerEngineBinding`].
//! The binding records either synchronous drop release or a pre-admitted bounded
//! finalizer contract. It is a provider declaration, not runtime verification.
//! The native `containerd::ContainerdEngine` conversion selects its verified
//! pre-admitted finalizer contract automatically.

mod config;
pub use config::ContainerRunnerConfig;

mod engine;
pub use engine::{
    ContainerAttempt, ContainerEngine, ContainerEngineBinding, ContainerEngineError,
    ContainerEngineInfo, ContainerErrorClass, ContainerExitStatus, ContainerOutput,
    ContainerOwnershipContract, ContainerRequest,
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
/// Custom engines must pass an explicit [`ContainerEngineBinding`].
/// A concrete native `containerd::ContainerdEngine` handle converts automatically
/// when the `containerd` feature is enabled.
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use solti_exec::container::{
///     ContainerEngine, ContainerEngineBinding, register_container_runner,
/// };
/// use solti_runner::RunnerRouter;
///
/// fn register_custom(
///     router: &mut RunnerRouter,
///     engine: Arc<dyn ContainerEngine>,
/// ) -> Result<(), solti_exec::ExecError> {
///     let engine = ContainerEngineBinding::drop_releases(engine);
///     register_container_runner(router, "custom", engine)
/// }
/// ```
///
/// A raw custom trait object does not satisfy the registration boundary:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use solti_exec::container::{ContainerEngine, register_container_runner};
/// use solti_runner::RunnerRouter;
///
/// fn register_without_contract(
///     router: &mut RunnerRouter,
///     engine: Arc<dyn ContainerEngine>,
/// ) {
///     register_container_runner(router, "custom", engine).unwrap();
/// }
/// ```
///
/// # Errors
///
/// Returns [`crate::ExecError`] when configuration or registration fails.
pub fn register_container_runner(
    router: &mut RunnerRouter,
    name: impl Into<String>,
    engine: impl Into<ContainerEngineBinding>,
) -> Result<(), crate::ExecError> {
    register_runner(router, Arc::new(ContainerRunner::new(name, engine.into())?))
}

/// Registers a container runner with explicit settings.
///
/// The runner receives label `solti.io/runner-name=<name>`.
/// Custom engines must pass an explicit [`ContainerEngineBinding`].
///
/// # Errors
///
/// Returns [`crate::ExecError`] when configuration or registration fails.
pub fn register_container_runner_with_config(
    router: &mut RunnerRouter,
    name: impl Into<String>,
    engine: impl Into<ContainerEngineBinding>,
    config: ContainerRunnerConfig,
) -> Result<(), crate::ExecError> {
    register_runner(
        router,
        Arc::new(ContainerRunner::with_config(name, engine.into(), config)?),
    )
}

/// Binds the native engine to its source-verified bounded finalizer contract.
#[cfg(feature = "containerd")]
impl From<Arc<containerd::ContainerdEngine>> for ContainerEngineBinding {
    fn from(engine: Arc<containerd::ContainerdEngine>) -> Self {
        let engine: Arc<dyn ContainerEngine> = engine;
        Self::pre_admitted_finalizer(engine)
    }
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
