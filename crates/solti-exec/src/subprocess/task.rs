//! # Resolved subprocess task
//!
//! The runner resolves a resource and [`BuildContext`](solti_runner::BuildContext) into immutable settings.
//! Every attempt reuses those settings.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use solti_model::Flag;

use crate::subprocess::backend::validate_env_name;

/// Immutable settings for one subprocess task.
#[derive(Clone)]
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
        reject_nul("subprocess command", &self.command)?;
        for arg in &self.args {
            reject_nul("subprocess argument", arg)?;
        }
        if let Some(cwd) = &self.cwd {
            reject_nul("subprocess cwd", &cwd.to_string_lossy())?;
        }
        for (name, value) in &self.env {
            validate_env_name(name)?;
            validate_env_value(name, value)?;
        }
        Ok(())
    }
}

/// Refuses a NUL byte in `value`, which the operating system cannot pass on.
///
/// `what` names the value in the error.
pub(crate) fn reject_nul(what: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{what} contains NUL"));
    }
    Ok(())
}

/// Refuses a NUL byte in the value of the environment variable `name`.
pub(crate) fn validate_env_value(name: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!(
            "environment variable {name:?} contains a NUL value"
        ));
    }
    Ok(())
}

impl fmt::Debug for SubprocessTaskConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubprocessTaskConfig")
            .field("run_id", &self.run_id)
            .field("seq", &self.seq)
            .field("argument_count", &self.args.len())
            .field("environment_count", &self.env.len())
            .field("cwd_set", &self.cwd.is_some())
            .field("fail_on_non_zero", &self.fail_on_non_zero.is_enabled())
            .finish()
    }
}

impl fmt::Display for SubprocessTaskConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SubprocessTaskConfig(args={}, env={}, cwd_set={}, fail_on_non_zero={})",
            self.args.len(),
            self.env.len(),
            self.cwd.is_some(),
            self.fail_on_non_zero.is_enabled(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_redact_process_inputs() {
        let config = SubprocessTaskConfig {
            run_id: Arc::from("format-run"),
            seq: 7,
            command: "command-secret".into(),
            args: vec!["argument-secret".into()],
            env: BTreeMap::from([("TOKEN".into(), "environment-secret".into())]),
            cwd: Some(PathBuf::from("/cwd-secret")),
            fail_on_non_zero: Flag::enabled(),
        };

        for formatted in [format!("{config:?}"), config.to_string()] {
            for secret in [
                "command-secret",
                "argument-secret",
                "TOKEN",
                "environment-secret",
                "cwd-secret",
            ] {
                assert!(!formatted.contains(secret), "{formatted}");
            }
            assert!(formatted.contains("args") || formatted.contains("argument_count"));
            assert!(formatted.contains("env") || formatted.contains("environment_count"));
        }
    }
}
