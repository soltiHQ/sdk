use std::{fmt, path::PathBuf};

use tracing::trace;
use tno_model::{Env, Flag};

use crate::ExecError;

/// Internal configuration for a subprocess.
#[derive(Debug, Clone)]
pub struct SubprocessConfig {
    /// Command to execute (e.g. `"ls"`, `"/usr/bin/python"`).
    pub(crate) command: String,
    /// Command-line arguments passed to the command.
    pub(crate) args: Vec<String>,
    /// Final merged environment for the subprocess.
    ///
    /// Usually this is `BuildContext.env()` merged with the `Env` from `TaskKind::Exec`, where task-level entries override context ones.
    pub(crate) env: Env,
    /// Working directory for the subprocess.
    ///
    /// If `None`, the subprocess inherits the parent process working directory.
    pub(crate) cwd: Option<PathBuf>,
    /// Whether non-zero exit codes should be treated as task failures.
    pub(crate) fail_on_non_zero: Flag,
}

impl SubprocessConfig {
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

    /// Emit a trace-level log with the essential configuration fields.
    pub fn trace_state(&self, slot: &str) {
        trace!(
            target: "tno.exec.subprocess.config",
            slot = slot,
            command = %self.command,
            args = ?self.args,
            cwd = ?self.cwd,
            env_len = self.env.len(),
            fail_on_non_zero = self.fail_on_non_zero.is_enabled(),
            "prepared subprocess config"
        );
    }
}

impl fmt::Display for SubprocessConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SubprocessConfig(cmd='{}', args={}, env={}, cwd={:?}, fail_on_non_zero={})",
            self.command,
            self.args.len(),
            self.env.len(),
            self.cwd,
            self.fail_on_non_zero.is_enabled(),
        )
    }
}