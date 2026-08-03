//! # Resolved subprocess task
//!
//! The runner resolves a resource and [`BuildContext`](solti_runner::BuildContext) into immutable settings.
//! Every attempt reuses those settings.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use solti_model::Flag;

use crate::subprocess::backend::validate_env_name;

/// Immutable settings for one subprocess task.
#[derive(Debug, Clone)]
pub(crate) struct SubprocessTaskConfig {
    /// Taskvisor run identifier.
    pub(crate) run_id: Arc<str>,
    /// Run sequence used in cgroup names.
    pub(crate) seq: u64,
    /// Executable name or path.
    pub(crate) command: String,
    /// Command-line arguments passed to the command.
    pub(crate) args: Vec<String>,
    /// Merged task and runner environment.
    pub(crate) env: BTreeMap<String, String>,
    /// Initial working directory.
    ///
    /// If `None`, the subprocess inherits the parent process working directory.
    pub(crate) cwd: Option<PathBuf>,
    /// Whether a non-zero exit is a retryable task failure.
    pub(crate) fail_on_non_zero: Flag,
}

impl SubprocessTaskConfig {
    /// Validates values passed to the operating system.
    ///
    /// Empty commands and embedded NUL bytes are rejected.
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
