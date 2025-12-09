use std::{fmt, path::PathBuf};

use tno_model::{Flag, TaskEnv};

use crate::ExecError;

/// Task configuration for a subprocess.
///
/// Describe parameters for task execution via subprocess.
#[derive(Debug, Clone)]
pub struct SubprocessTaskConfig {
    /// End-to-End log identifier.
    pub(crate) run_id: String,
    /// Command to execute (e.g. `"ls"`, `"/usr/bin/python"`).
    pub(crate) command: String,
    /// Command-line arguments passed to the command.
    pub(crate) args: Vec<String>,
    /// Environment for the subprocess.
    pub(crate) env: TaskEnv,
    /// Working directory for the subprocess.
    ///
    /// If `None`, the subprocess inherits the parent process working directory.
    pub(crate) cwd: Option<PathBuf>,
    /// Whether non-zero exit codes should be treated as task failures.
    pub(crate) fail_on_non_zero: Flag,
}

impl SubprocessTaskConfig {
    /// Validate the configuration before spawning a subprocess.
    ///
    /// Rules:
    /// - `command` is not empty or whitespace-only.
    pub fn validate(&self) -> Result<(), ExecError> {
        if self.command.trim().is_empty() {
            return Err(ExecError::InvalidSpec("Subprocess command is empty".into()));
        }
        Ok(())
    }
}

impl fmt::Display for SubprocessTaskConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SubprocessTaskConfig(cmd='{}', args={}, env={}, cwd={:?}, fail_on_non_zero={})",
            self.command,
            self.args.len(),
            self.env.len(),
            self.cwd,
            self.fail_on_non_zero.is_enabled(),
        )
    }
}
