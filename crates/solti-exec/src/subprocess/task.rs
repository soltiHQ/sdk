//! Resolved configuration for one subprocess task.
//!
//! The runner builds this value from the resource and [`BuildContext`](solti_runner::BuildContext).
//! Attempts reuse it without reading mutable model state.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use solti_model::Flag;

use crate::subprocess::backend::validate_env_name;

/// Subprocess configuration - fully resolved per-task parameters.
///
/// ## Also
///
/// - [`SubprocessRunner`](super::SubprocessRunner) produces this config in `build_task`.
/// - [`SubprocessBackendConfig`](super::SubprocessBackendConfig) runner-level settings applied at spawn.
#[derive(Debug, Clone)]
pub(crate) struct SubprocessTaskConfig {
    /// End-to-End log identifier.
    pub(crate) run_id: Arc<str>,
    /// Raw sequence number from run id generation (used for cgroup naming).
    pub(crate) seq: u64,
    /// Command to execute (e.g. `"ls"`, `"/usr/bin/python"`).
    pub(crate) command: String,
    /// Command-line arguments passed to the command.
    pub(crate) args: Vec<String>,
    /// Environment from the resource and build context.
    pub(crate) env: BTreeMap<String, String>,
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
    /// Rejects empty commands and values that cannot be passed to `execve`.
    pub fn validate(&self) -> Result<(), String> {
        if self.command.trim().is_empty() {
            return Err("subprocess command is empty".into());
        }
        if self.command.contains('\0') {
            return Err("subprocess command contains NUL".into());
        }
        if self.args.iter().any(|arg| arg.contains('\0')) {
            return Err("subprocess argument contains NUL".into());
        }
        if self
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.to_string_lossy().contains('\0'))
        {
            return Err("subprocess cwd contains NUL".into());
        }
        for (name, value) in &self.env {
            validate_env_name(name)?;
            if value.contains('\0') {
                return Err(format!(
                    "environment variable {name:?} contains a NUL value"
                ));
            }
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
