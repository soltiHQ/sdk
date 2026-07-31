//! # Subprocess backend
//!
//! [`SubprocessBackendConfig`] contains subprocess-specific settings.
//! [`HostProcessPolicy`] contains operating-system process controls.
//!
//! ## Flow
//!
//! ```text
//! SubprocessBackendConfig + HostProcessPolicy
//!      │ runner construction
//!      ├── validate platform and values
//!      └── resolve cwd roots and cgroup parent
//!                 │
//!                 ▼
//!          each task attempt
//!      ├── build environment and cwd
//!      ├── prepare cgroup
//!      ├── attach rlimits and security
//!      └── stream output
//! ```
//!
//! Configuration setters do not perform validation.
//! [`SubprocessRunner::with_config`](super::SubprocessRunner::with_config) validates the complete configuration.

use std::path::{Path, PathBuf};

use solti_model::MAX_SCRIPT_BODY_BYTES;
use tokio::process::Command;

use crate::ExecError::InvalidRunnerConfig;
use crate::host::{
    HostProcessGuard, HostProcessPolicy, PreparedHostProcessAttempt, PreparedHostProcessPolicy,
};
use crate::subprocess::logger::LogConfig;

/// Minimal `PATH` used for a cleared environment.
pub(crate) const SAFE_DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub(crate) fn validate_env_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("environment variable name cannot be empty".into());
    }
    if name.contains('=') || name.contains('\0') {
        return Err(format!(
            "invalid environment variable name {name:?}: '=' and NUL are not allowed"
        ));
    }
    Ok(())
}

/// How the child process's environment is built.
///
/// The default is [`Clear`](Self::Clear).
/// Task and runner values are applied after this policy.
/// Runner values win on duplicate keys.
#[derive(Debug, Clone, Default)]
pub enum EnvPolicy {
    /// Inherit the agent environment.
    Inherit,
    /// Clear the agent environment.
    ///
    /// A safe `PATH` is added when the merged task environment does not set one.
    #[default]
    Clear,
    /// Copy only the named variables from the agent environment.
    ///
    /// A safe `PATH` is added when the allowlist does not name `PATH` and the merged values do not set it.
    /// Invalid environment names are rejected when the runner is created.
    Allowlist(Vec<String>),
}

/// Where a task is allowed to set its working directory.
///
/// [`Roots`](Self::Roots) checks the starting directory at build time.
/// This policy does not restrict later file access.
#[derive(Debug, Clone, Default)]
pub enum CwdPolicy {
    /// Accept any task working directory.
    #[default]
    Unrestricted,
    /// Require an explicit task working directory under one of these roots.
    ///
    /// Roots are canonicalized when the runner is created.
    /// The task directory is canonicalized when the task is built.
    Roots(Vec<PathBuf>),
}

impl CwdPolicy {
    fn prepare(&mut self) -> Result<(), crate::ExecError> {
        let CwdPolicy::Roots(roots) = self else {
            return Ok(());
        };
        if roots.is_empty() {
            return Err(InvalidRunnerConfig(
                "cwd roots policy requires at least one root".into(),
            ));
        }

        let mut prepared = Vec::with_capacity(roots.len());
        for root in roots.iter() {
            let real = root.canonicalize().map_err(|e| {
                InvalidRunnerConfig(format!(
                    "cwd root {} cannot be resolved: {e}",
                    root.display()
                ))
            })?;
            if !real.is_dir() {
                return Err(InvalidRunnerConfig(format!(
                    "cwd root {} is not a directory",
                    real.display()
                )));
            }
            prepared.push(real);
        }
        prepared.sort();
        prepared.dedup();
        *roots = prepared;
        Ok(())
    }

    /// Checks a task working directory against the policy.
    ///
    /// Canonicalization resolves symlinks and `..` before root comparison.
    fn check(&self, cwd: Option<&Path>) -> Result<(), String> {
        let CwdPolicy::Roots(roots) = self else {
            return Ok(());
        };
        let Some(cwd) = cwd else {
            return Err(
                "cwd is required under a Roots policy; a task may not inherit the agent's cwd"
                    .into(),
            );
        };

        let real = cwd
            .canonicalize()
            .map_err(|e| format!("cwd {} cannot be resolved: {e}", cwd.display()))?;
        if !real.is_dir() {
            return Err(format!("cwd {} is not a directory", real.display()));
        }
        let allowed = roots.iter().any(|root| real.starts_with(root));
        if !allowed {
            return Err(format!(
                "cwd {} is outside the allowed roots",
                real.display()
            ));
        }
        Ok(())
    }
}

/// Environment, working-directory, output, and script settings for a runner.
///
/// | Setting                  | Default                          |
/// |--------------------------|----------------------------------|
/// | Host process policy      | Empty                            |
/// | Environment              | [`EnvPolicy::Clear`]             |
/// | Working directory        | [`CwdPolicy::Unrestricted`]      |
/// | Output logging           | [`LogConfig::default`]           |
/// | Decoded script body      | [`MAX_SCRIPT_BODY_BYTES`]        |
///
/// ## Example
///
/// ```rust
/// use solti_exec::host::HostProcessPolicy;
/// use solti_exec::subprocess::{EnvPolicy, LogConfig, SubprocessBackendConfig};
///
/// let backend = SubprocessBackendConfig::new()
///     .with_host_process_policy(HostProcessPolicy::new())
///     .with_env_policy(EnvPolicy::Clear)
///     .with_logger(LogConfig {
///         max_line_length: 2048,
///         ..Default::default()
///     })
///     .with_max_script_body_bytes(256 * 1024);
/// ```
///
/// ## See Also
///
/// - [`SubprocessRunner::with_config`](super::SubprocessRunner::with_config)
/// - [`register_subprocess_runner_with_backend`](super::register_subprocess_runner_with_backend)
#[derive(Debug, Clone, Default)]
pub struct SubprocessBackendConfig {
    /// Controls applied to the host process.
    host_process_policy: HostProcessPolicy,
    /// How the child environment is built.
    env_policy: EnvPolicy,
    /// Where a task may set its working directory.
    cwd_policy: CwdPolicy,
    /// Subprocess output logging configuration.
    logger: LogConfig,
    /// Maximum decoded script size.
    max_script_body_bytes: Option<usize>,
}

impl SubprocessBackendConfig {
    /// Creates the default backend configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets controls applied to every host process attempt.
    pub fn with_host_process_policy(mut self, policy: HostProcessPolicy) -> Self {
        self.host_process_policy = policy;
        self
    }

    /// Sets the child environment policy.
    pub fn with_env_policy(mut self, policy: EnvPolicy) -> Self {
        self.env_policy = policy;
        self
    }

    /// Sets the task working-directory policy.
    ///
    /// [`CwdPolicy::Roots`] rejects a task outside the configured roots.
    pub fn with_cwd_policy(mut self, policy: CwdPolicy) -> Self {
        self.cwd_policy = policy;
        self
    }

    /// Sets subprocess output configuration.
    ///
    /// Both size limits must be greater than zero.
    pub fn with_logger(mut self, config: LogConfig) -> Self {
        self.logger = config;
        self
    }

    /// Sets the maximum decoded script size.
    ///
    /// The value must be between `1` and [`MAX_SCRIPT_BODY_BYTES`].
    /// It can lower the model limit but cannot raise it.
    /// Oversized scripts are rejected when the task is built.
    pub fn with_max_script_body_bytes(mut self, max: usize) -> Self {
        self.max_script_body_bytes = Some(max);
        self
    }

    /// Validates and normalizes the complete configuration.
    pub(crate) fn prepare(mut self) -> Result<PreparedSubprocessBackendConfig, crate::ExecError> {
        self.cwd_policy.prepare()?;

        if let EnvPolicy::Allowlist(keys) = &self.env_policy {
            for key in keys {
                validate_env_name(key).map_err(InvalidRunnerConfig)?;
            }
        }

        if self.logger.max_line_length == 0 {
            return Err(InvalidRunnerConfig(
                "log_config.max_line_length cannot be zero".into(),
            ));
        }
        if self.logger.max_line_bytes == 0 {
            return Err(InvalidRunnerConfig(
                "log_config.max_line_bytes cannot be zero (all output would be swallowed)".into(),
            ));
        }
        if let Some(max) = self.max_script_body_bytes
            && (max == 0 || max > MAX_SCRIPT_BODY_BYTES)
        {
            return Err(InvalidRunnerConfig(format!(
                "max_script_body_bytes must be in 1..={MAX_SCRIPT_BODY_BYTES}, got {max}"
            )));
        }
        let host_process_policy = self.host_process_policy.prepare()?;

        Ok(PreparedSubprocessBackendConfig {
            host_process_policy,
            env_policy: self.env_policy,
            cwd_policy: self.cwd_policy,
            logger: self.logger,
            max_script_body_bytes: self.max_script_body_bytes,
        })
    }
}

/// Subprocess backend configuration prepared during runner construction.
#[derive(Debug)]
pub(crate) struct PreparedSubprocessBackendConfig {
    host_process_policy: PreparedHostProcessPolicy,
    env_policy: EnvPolicy,
    cwd_policy: CwdPolicy,
    logger: LogConfig,
    max_script_body_bytes: Option<usize>,
}

impl PreparedSubprocessBackendConfig {
    /// Validates a task working directory against the configured policy.
    pub(crate) fn check_cwd(&self, cwd: Option<&Path>) -> Result<(), String> {
        self.cwd_policy.check(cwd)
    }

    /// Returns the effective decoded script limit.
    pub(crate) fn max_script_body_bytes(&self) -> usize {
        self.max_script_body_bytes.unwrap_or(MAX_SCRIPT_BODY_BYTES)
    }

    /// Returns the child environment policy.
    pub(crate) fn env_policy(&self) -> &EnvPolicy {
        &self.env_policy
    }

    /// Returns subprocess output configuration.
    pub(crate) fn log_config(&self) -> &LogConfig {
        &self.logger
    }

    /// Returns `true` when cgroup limits are configured.
    pub(crate) fn has_cgroups(&self) -> bool {
        self.host_process_policy.has_cgroups()
    }

    /// Prepares host process resources for one attempt.
    ///
    /// This must run before [`apply_to_command`](Self::apply_to_command).
    pub(crate) fn prepare_host_process_attempt(
        &self,
        cgroup_name: Option<&str>,
    ) -> Result<PreparedHostProcessAttempt, crate::ExecError> {
        self.host_process_policy
            .prepare_attempt(cgroup_name)
            .map_err(Into::into)
    }

    /// Attaches configured controls to a command.
    ///
    /// The hooks apply rlimits, cgroup membership, and security before `execve`.
    /// The cgroup must already be prepared.
    pub(crate) fn apply_to_command(
        &self,
        cmd: &mut Command,
        attempt: PreparedHostProcessAttempt,
    ) -> HostProcessGuard {
        attempt.apply_to_command(cmd.as_std_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_log_limits_are_rejected() {
        let cases = [
            (
                LogConfig {
                    max_line_length: 0,
                    ..LogConfig::default()
                },
                "max_line_length",
            ),
            (
                LogConfig {
                    max_line_bytes: 0,
                    ..LogConfig::default()
                },
                "max_line_bytes",
            ),
        ];

        for (logger, expected) in cases {
            let error = SubprocessBackendConfig::new()
                .with_logger(logger)
                .prepare()
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "got {error:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn host_process_policy_is_applied_to_spawned_command() {
        use crate::host::{HostProcessPolicy, RlimitConfig};

        let requested = crate::host::reduced_nofile_soft_limit_for_test();
        let config = SubprocessBackendConfig::new()
            .with_host_process_policy(HostProcessPolicy::new().with_rlimits(RlimitConfig {
                max_open_files: Some(requested),
                ..Default::default()
            }))
            .prepare()
            .unwrap();
        let attempt = config.prepare_host_process_attempt(None).unwrap();
        let mut command = Command::new("sh");
        command.arg("-c").arg("ulimit -n");
        let _guard = config.apply_to_command(&mut command, attempt);
        let output = command.output().await.unwrap();

        assert!(output.status.success());
        let actual = std::str::from_utf8(&output.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap();
        assert_eq!(actual, requested);
    }

    #[test]
    fn max_script_body_bytes_defaults_to_model_const_and_is_configurable() {
        use solti_model::MAX_SCRIPT_BODY_BYTES;

        let default_cfg = SubprocessBackendConfig::new().prepare().unwrap();
        assert_eq!(default_cfg.max_script_body_bytes(), MAX_SCRIPT_BODY_BYTES);

        let custom = SubprocessBackendConfig::new()
            .with_max_script_body_bytes(4096)
            .prepare()
            .unwrap();
        assert_eq!(custom.max_script_body_bytes(), 4096);
    }

    #[test]
    fn invalid_script_body_limits_are_rejected() {
        for max in [0, MAX_SCRIPT_BODY_BYTES + 1] {
            let error = SubprocessBackendConfig::new()
                .with_max_script_body_bytes(max)
                .prepare()
                .unwrap_err()
                .to_string();
            assert!(error.contains("1..="), "limit {max}: got {error:?}");
        }
    }

    #[test]
    fn env_policy_defaults_to_clear() {
        let cfg = SubprocessBackendConfig::new().prepare().unwrap();
        assert!(matches!(cfg.env_policy(), EnvPolicy::Clear));
    }

    #[test]
    fn invalid_allowlist_key_is_rejected() {
        let config = SubprocessBackendConfig::new()
            .with_env_policy(EnvPolicy::Allowlist(vec!["BAD=KEY".into()]));
        let error = config.prepare().unwrap_err().to_string();
        assert!(error.contains("environment variable name"), "got: {error}");
    }

    #[test]
    fn cwd_unrestricted_allows_anything() {
        let cfg = SubprocessBackendConfig::new().prepare().unwrap();
        assert!(cfg.check_cwd(None).is_ok());
        assert!(
            cfg.check_cwd(Some(Path::new("/nonexistent/anywhere")))
                .is_ok()
        );
    }

    #[test]
    fn cwd_roots_allows_paths_inside() {
        let root = tempfile::TempDir::new().unwrap();
        let sub = root.path().join("work");
        std::fs::create_dir(&sub).unwrap();

        let cfg = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
            .prepare()
            .unwrap();

        assert!(cfg.check_cwd(Some(&sub)).is_ok());
        assert!(cfg.check_cwd(Some(root.path())).is_ok());
    }

    #[test]
    fn cwd_roots_requires_explicit_cwd() {
        let root = tempfile::TempDir::new().unwrap();
        let cfg = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
            .prepare()
            .unwrap();

        let err = cfg.check_cwd(None).unwrap_err().to_string();
        assert!(err.contains("cwd is required"), "got: {err}");
    }

    #[test]
    fn cwd_roots_rejects_paths_outside() {
        let root = tempfile::TempDir::new().unwrap();
        let other = tempfile::TempDir::new().unwrap();

        let cfg = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
            .prepare()
            .unwrap();

        let err = cfg.check_cwd(Some(other.path())).unwrap_err().to_string();
        assert!(err.contains("outside the allowed roots"), "got: {err}");
    }

    #[test]
    fn cwd_roots_rejects_nonexistent() {
        let root = tempfile::TempDir::new().unwrap();
        let cfg = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
            .prepare()
            .unwrap();

        let missing = root.path().join("does-not-exist");
        let err = cfg.check_cwd(Some(&missing)).unwrap_err().to_string();
        assert!(err.contains("cannot be resolved"), "got: {err}");
    }

    #[test]
    fn cwd_roots_rejects_traversal_escape() {
        // A cwd built to look like it is under the root but that resolves out of
        // it via `..` must be rejected: canonicalize collapses the traversal.
        let base = tempfile::TempDir::new().unwrap();
        let root = base.path().join("root");
        let outside = base.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();

        let cfg = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![root.clone()]))
            .prepare()
            .unwrap();

        let escape = root.join("..").join("outside");
        let err = cfg.check_cwd(Some(&escape)).unwrap_err().to_string();
        assert!(err.contains("outside the allowed roots"), "got: {err}");
    }

    #[test]
    fn cwd_root_must_exist() {
        let config = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![PathBuf::from("/missing/solti-root")]));
        let error = config.prepare().unwrap_err().to_string();
        assert!(error.contains("cannot be resolved"), "got: {error}");
    }
}
