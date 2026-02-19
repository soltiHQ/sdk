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
use solti_model::{LABEL_RUNNER_TAG, RunnerLabels};

use crate::ExecError;

/// Register a subprocess runner with default settings.
pub fn register_subprocess_runner(
    router: &mut RunnerRouter,
    name: &'static str,
) -> Result<(), ExecError> {
    if router.contains_runner_tag(name) {
        return Err(ExecError::DuplicateRunnerTag {
            tag: name.to_string(),
        });
    }

    let mut labels = RunnerLabels::new();
    labels.insert(LABEL_RUNNER_TAG, name);
    router.register_with_labels(Arc::new(SubprocessRunner::new(name)), labels);
    Ok(())
}

/// Register a subprocess runner with explicit runner configuration.
pub fn register_subprocess_runner_with_backend(
    router: &mut RunnerRouter,
    name: &'static str,
    backend: SubprocessBackendConfig,
) -> Result<(), ExecError> {
    if router.contains_runner_tag(name) {
        return Err(ExecError::DuplicateRunnerTag {
            tag: name.to_string(),
        });
    }
    backend.validate()?;

    let mut labels = RunnerLabels::new();
    labels.insert(LABEL_RUNNER_TAG, name);
    router.register_with_labels(
        Arc::new(SubprocessRunner::with_config(name, backend)),
        labels,
    );
    Ok(())
}
