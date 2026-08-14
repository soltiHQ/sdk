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
    AttemptPrepareFailure, AttemptProcessDomain, HostProcessPolicy, PreparedHostProcessAttempt,
    PreparedHostProcessPolicy,
};
use crate::output::LogConfig;
use crate::subprocess::boundary::PinnedCwd;

/// Minimal `PATH` used for a cleared environment.
pub(crate) const SAFE_DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Default active and deferred subprocess cleanup ownership per runner.
pub const DEFAULT_SUBPROCESS_CLEANUP_CAPACITY: usize = 1024;

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
/// | Cleanup ownership        | 1024 attempts                    |
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
    /// Maximum active and deferred cleanup ownership.
    cleanup_capacity: Option<usize>,
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

    /// Sets the active and deferred subprocess cleanup ownership limit.
    ///
    /// Admission is reserved before script transport, cgroup, or process
    /// resources are created. Runner construction rejects zero and values that
    /// exceed the supported counter range.
    pub fn with_cleanup_capacity(mut self, capacity: usize) -> Self {
        self.cleanup_capacity = Some(capacity);
        self
    }

    /// Returns the configured subprocess cleanup ownership limit.
    pub fn cleanup_capacity(&self) -> usize {
        self.cleanup_capacity
            .unwrap_or(DEFAULT_SUBPROCESS_CLEANUP_CAPACITY)
    }

    /// Adds one owned file descriptor to the child passlist.
    ///
    /// The runner keeps the descriptor open at the same number.
    /// Descriptors `0`, `1`, and `2` are managed as standard streams.
    /// They are rejected when the runner is created.
    ///
    /// Linux marks every other open descriptor close-on-exec.
    /// The normal macOS path uses an atomic `posix_spawn` allowlist.
    /// Unix fallback paths apply the parent snapshot and child-side sweep described by the crate README.
    #[cfg(unix)]
    #[cfg_attr(docsrs, doc(cfg(unix)))]
    pub fn with_passed_fd(mut self, fd: OwnedFd) -> Self {
        self.passed_fds.push(Arc::new(fd));
        self
    }

    /// Validates and normalizes the complete configuration.
    pub(crate) fn prepare(self) -> Result<PreparedSubprocessBackendConfig, crate::ExecError> {
        let cleanup_capacity = self.cleanup_capacity();
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
        if cleanup_capacity == 0 || u32::try_from(cleanup_capacity).is_err() {
            return Err(InvalidRunnerConfig(
                "subprocess cleanup capacity is outside the supported range".into(),
            ));
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
            cleanup_capacity,
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
    cleanup_capacity: usize,
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

    /// Returns the active and deferred cleanup ownership limit.
    pub(crate) fn prepared_cleanup_capacity(&self) -> usize {
        self.cleanup_capacity
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
    #[cfg(test)]
    pub(crate) fn prepare_host_process_attempt(
        &self,
        cgroup_name: Option<&str>,
    ) -> Result<PreparedHostProcessAttempt, crate::ExecError> {
        self.host_process_policy
            .prepare_attempt(cgroup_name)
            .map_err(Into::into)
    }

    /// Prepares host resources without erasing residual cleanup ownership.
    pub(crate) fn prepare_host_process_attempt_owned(
        &self,
        cgroup_name: Option<&str>,
    ) -> Result<PreparedHostProcessAttempt, AttemptPrepareFailure> {
        self.host_process_policy.prepare_attempt_owned(cgroup_name)
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
mod tests;
