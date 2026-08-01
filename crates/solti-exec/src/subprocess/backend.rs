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
//!                 ▼
//!          each task attempt
//!      ├── build environment
//!      ├── use pinned cwd
//!      ├── prepare host resources
//!      ├── attach process controls
//!      └── stream output
//! ```
//!
//! Configuration setters do not perform validation.
//! [`SubprocessRunner::with_config`](super::SubprocessRunner::with_config) validates the complete configuration.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(unix)]
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};

use solti_model::MAX_SCRIPT_BODY_BYTES;
use tokio::process::Command;

use crate::ExecError::InvalidRunnerConfig;
use crate::host::{
    AttemptProcessDomain, HostProcessPolicy, PreparedHostProcessAttempt, PreparedHostProcessPolicy,
};
use crate::output::LogConfig;
use crate::subprocess::boundary::PinnedCwd;

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
/// An explicit directory is opened and pinned at build time.
/// [`Roots`](Self::Roots) also checks it against pinned roots.
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
    fn prepare(self) -> Result<PreparedCwdPolicy, crate::ExecError> {
        match self {
            CwdPolicy::Unrestricted => Ok(PreparedCwdPolicy::Unrestricted),
            CwdPolicy::Roots(roots) => {
                if roots.is_empty() {
                    return Err(InvalidRunnerConfig(
                        "cwd roots policy requires at least one root".into(),
                    ));
                }

                let mut prepared = Vec::with_capacity(roots.len());
                for root in roots {
                    let real = resolve_directory(&root, "cwd root").map_err(InvalidRunnerConfig)?;
                    let directory = PinnedCwd::open_absolute(&real).map_err(|error| {
                        InvalidRunnerConfig(format!(
                            "cwd root {} cannot be pinned: {error}",
                            real.display()
                        ))
                    })?;
                    prepared.push(PreparedCwdRoot {
                        path: real,
                        directory,
                    });
                }
                prepared.sort_by(|left, right| left.path.cmp(&right.path));
                prepared.dedup_by(|left, right| left.path == right.path);
                Ok(PreparedCwdPolicy::Roots(prepared))
            }
        }
    }
}

#[derive(Debug)]
enum PreparedCwdPolicy {
    Unrestricted,
    Roots(Vec<PreparedCwdRoot>),
}

#[derive(Debug)]
struct PreparedCwdRoot {
    path: PathBuf,
    directory: PinnedCwd,
}

impl PreparedCwdPolicy {
    /// Resolves, validates, and pins a task working directory.
    fn pin(&self, cwd: Option<&Path>) -> Result<Option<PinnedCwd>, String> {
        let Some(cwd) = cwd else {
            return match self {
                Self::Unrestricted => Ok(None),
                Self::Roots(_) => Err(
                    "cwd is required under a Roots policy; a task may not inherit the agent's cwd"
                        .into(),
                ),
            };
        };

        let real = resolve_directory(cwd, "cwd")?;
        match self {
            Self::Unrestricted => PinnedCwd::open_absolute(&real)
                .map(Some)
                .map_err(|error| format!("cwd {} cannot be pinned: {error}", real.display())),
            Self::Roots(roots) => {
                let root = roots
                    .iter()
                    .filter(|root| real.starts_with(&root.path))
                    .max_by_key(|root| root.path.components().count())
                    .ok_or_else(|| {
                        format!("cwd {} is outside the allowed roots", real.display())
                    })?;
                let relative = real
                    .strip_prefix(&root.path)
                    .map_err(|_| format!("cwd {} is outside the allowed roots", real.display()))?;
                root.directory
                    .open_beneath(relative)
                    .map(Some)
                    .map_err(|error| format!("cwd {} cannot be pinned: {error}", real.display()))
            }
        }
    }
}

fn resolve_directory(path: &Path, field: &str) -> Result<PathBuf, String> {
    let real = path
        .canonicalize()
        .map_err(|error| format!("{field} {} cannot be resolved: {error}", path.display()))?;
    if !real.is_dir() {
        return Err(format!("{field} {} is not a directory", real.display()));
    }
    Ok(real)
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
/// | Extra inherited FDs      | Empty                            |
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
    /// Owned descriptors explicitly inherited by every subprocess.
    #[cfg(unix)]
    passed_fds: Vec<Arc<OwnedFd>>,
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
    ///
    /// Oversized scripts are rejected when the task is built.
    pub fn with_max_script_body_bytes(mut self, max: usize) -> Self {
        self.max_script_body_bytes = Some(max);
        self
    }

    /// Adds one owned file descriptor to the child passlist.
    ///
    /// The runner keeps the descriptor open at the same number.
    /// Descriptors `0`, `1`, and `2` are managed as standard streams.
    /// They are rejected when the runner is created.
    ///
    /// Linux marks every other open descriptor close-on-exec.
    /// Other Unix platforms apply the bounded snapshot described by the crate README.
    #[cfg(unix)]
    #[cfg_attr(docsrs, doc(cfg(unix)))]
    pub fn with_passed_fd(mut self, fd: OwnedFd) -> Self {
        self.passed_fds.push(Arc::new(fd));
        self
    }

    /// Validates and normalizes the complete configuration.
    pub(crate) fn prepare(self) -> Result<PreparedSubprocessBackendConfig, crate::ExecError> {
        let cwd_policy = self.cwd_policy.prepare()?;

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
        #[cfg(unix)]
        for fd in &self.passed_fds {
            if fd.as_raw_fd() < 3 {
                return Err(InvalidRunnerConfig(format!(
                    "passed file descriptor must be at least 3, got {}",
                    fd.as_raw_fd()
                )));
            }
        }
        let host_process_policy = self.host_process_policy.prepare()?;

        Ok(PreparedSubprocessBackendConfig {
            host_process_policy,
            env_policy: self.env_policy,
            cwd_policy,
            logger: self.logger,
            max_script_body_bytes: self.max_script_body_bytes,
            #[cfg(unix)]
            passed_fds: self.passed_fds,
        })
    }
}

/// Subprocess backend configuration prepared during runner construction.
#[derive(Debug)]
pub(crate) struct PreparedSubprocessBackendConfig {
    host_process_policy: PreparedHostProcessPolicy,
    env_policy: EnvPolicy,
    cwd_policy: PreparedCwdPolicy,
    logger: LogConfig,
    max_script_body_bytes: Option<usize>,
    #[cfg(unix)]
    passed_fds: Vec<Arc<OwnedFd>>,
}

impl PreparedSubprocessBackendConfig {
    /// Resolves, validates, and pins a task working directory.
    pub(crate) fn pin_cwd(&self, cwd: Option<&Path>) -> Result<Option<PinnedCwd>, String> {
        self.cwd_policy.pin(cwd)
    }

    /// Returns the descriptors explicitly inherited by every subprocess.
    #[cfg(unix)]
    pub(crate) fn passed_fds(&self) -> Vec<RawFd> {
        self.passed_fds.iter().map(|fd| fd.as_raw_fd()).collect()
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

    /// Returns `true` when subprocesses create a new Unix session.
    pub(crate) fn starts_new_session(&self) -> bool {
        self.host_process_policy.starts_new_session()
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
    /// The hooks apply process state, rlimits, cgroup membership, and security.
    /// The cgroup must already be prepared.
    pub(crate) fn apply_to_command(
        &self,
        cmd: &mut Command,
        attempt: PreparedHostProcessAttempt,
    ) -> AttemptProcessDomain {
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

        let requested = crate::host::reduced_nofile_limit_for_test();
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
    fn cwd_unrestricted_allows_inherited_or_existing_directory() {
        let cfg = SubprocessBackendConfig::new().prepare().unwrap();
        assert!(cfg.pin_cwd(None).unwrap().is_none());

        let cwd = tempfile::TempDir::new().unwrap();
        assert!(cfg.pin_cwd(Some(cwd.path())).unwrap().is_some());
    }

    #[test]
    fn cwd_unrestricted_rejects_nonexistent_directory() {
        let cfg = SubprocessBackendConfig::new().prepare().unwrap();
        let error = cfg
            .pin_cwd(Some(Path::new("/nonexistent/solti-cwd")))
            .unwrap_err();
        assert!(error.contains("cannot be resolved"), "got: {error}");
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

        assert!(cfg.pin_cwd(Some(&sub)).unwrap().is_some());
        assert!(cfg.pin_cwd(Some(root.path())).unwrap().is_some());
    }

    #[test]
    fn cwd_roots_requires_explicit_cwd() {
        let root = tempfile::TempDir::new().unwrap();
        let cfg = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
            .prepare()
            .unwrap();

        let err = cfg.pin_cwd(None).unwrap_err().to_string();
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

        let err = cfg.pin_cwd(Some(other.path())).unwrap_err().to_string();
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
        let err = cfg.pin_cwd(Some(&missing)).unwrap_err().to_string();
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
        let err = cfg.pin_cwd(Some(&escape)).unwrap_err().to_string();
        assert!(err.contains("outside the allowed roots"), "got: {err}");
    }

    #[test]
    fn cwd_root_must_exist() {
        let config = SubprocessBackendConfig::new()
            .with_cwd_policy(CwdPolicy::Roots(vec![PathBuf::from("/missing/solti-root")]));
        let error = config.prepare().unwrap_err().to_string();
        assert!(error.contains("cannot be resolved"), "got: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn passed_fd_is_owned_by_prepared_backend() {
        use std::os::fd::AsRawFd as _;

        let file = tempfile::tempfile().unwrap();
        let expected = file.as_raw_fd();
        let cfg = SubprocessBackendConfig::new()
            .with_passed_fd(file.into())
            .prepare()
            .unwrap();

        assert_eq!(cfg.passed_fds(), vec![expected]);
    }
}
