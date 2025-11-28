//! Subprocess runner for `tno_model::TaskKind::Subprocess`.

mod config;
mod runner;

pub use runner::SubprocessRunner;

use crate::ExecError;
use std::sync::Arc;
use tno_core::RunnerRouter;
use tno_model::{LABEL_RUNNER_TAG, Labels};

/// Register the built-in subprocess runner in the given router.
pub fn register_subprocess_runner(
    router: &mut RunnerRouter,
    name: &'static str,
) -> Result<(), ExecError> {
    if router.contains_runner_tag(name) {
        return Err(ExecError::DuplicateRunnerTag {
            tag: name.to_string(),
        });
    }
    let mut labels = Labels::new();
    labels.insert(LABEL_RUNNER_TAG, name);

    router.register_with_labels(Arc::new(SubprocessRunner::new(name)), labels);
    Ok(())
}
