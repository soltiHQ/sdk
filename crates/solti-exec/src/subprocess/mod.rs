//! Subprocess runner for `solti_model::TaskKind::Subprocess`.
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
    if router.contains_label(LABEL_RUNNER_NAME, name) {
        return Err(ExecError::DuplicateRunner {
            name: name.to_string(),
        });
    }

    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_NAME, name);
    router.register_with_labels(Arc::new(SubprocessRunner::new(name)), labels);
    Ok(())
}

/// Register a subprocess runner with explicit runner configuration.
pub fn register_subprocess_runner_with_backend(
    router: &mut RunnerRouter,
    name: &'static str,
    backend: SubprocessBackendConfig,
) -> Result<(), ExecError> {
    if router.contains_label(LABEL_RUNNER_NAME, name) {
        return Err(ExecError::DuplicateRunner {
            name: name.to_string(),
        });
    }
    backend.validate()?;

    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_NAME, name);
    router.register_with_labels(
        Arc::new(SubprocessRunner::with_config(name, backend)),
        labels,
    );
    Ok(())
}
