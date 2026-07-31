//! # Subprocess backend
//!
//! [`SubprocessBackendConfig`] contains settings shared by one runner.
//! Each spawned process receives those settings.
//!
//! ## Flow
//!
//! ```text
//! SubprocessBackendConfig
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

use tokio::process::Command;
use tracing::trace;

use solti_model::MAX_SCRIPT_BODY_BYTES;

use crate::ExecError::InvalidRunnerConfig;
use crate::subprocess::logger::LogConfig;
use crate::utils::{CgroupLimits, PreparedCgroup, RlimitConfig, SecurityConfig};
use crate::utils::{attach_cgroup, attach_rlimits, attach_security};

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

/// Resource, security, environment, and output settings for a runner.
///
/// | Setting                  | Default                          |
/// |--------------------------|----------------------------------|
/// | POSIX rlimits            | Disabled                         |
/// | Linux cgroup v2          | Disabled                         |
/// | Linux security           | Disabled                         |
/// | Environment              | [`EnvPolicy::Clear`]             |
/// | Working directory        | [`CwdPolicy::Unrestricted`]      |
/// | Output logging           | [`LogConfig::default`]           |
/// | Decoded script body      | [`MAX_SCRIPT_BODY_BYTES`]        |
///
/// Non-empty rlimits require Unix.
/// Cgroups and security controls require Linux.
///
/// ## Example
///
/// ```rust
/// use solti_exec::subprocess::{EnvPolicy, LogConfig, SubprocessBackendConfig};
///
/// let backend = SubprocessBackendConfig::new()
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
    /// POSIX rlimit-based resource limits.
    rlimits: Option<RlimitConfig>,
    /// Linux cgroup v2 resource limits.
    cgroups: Option<CgroupLimits>,
    /// Parent directory for per-attempt cgroups.
    cgroup_parent: Option<PathBuf>,
    /// Security hardening.
    security: Option<SecurityConfig>,
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

    /// Sets POSIX process limits.
    ///
    /// A non-empty configuration requires Unix.
    pub fn with_rlimits(mut self, rlimits: RlimitConfig) -> Self {
        self.rlimits = Some(rlimits);
        self
    }

    /// Sets Linux cgroup v2 limits.
    ///
    /// This setting requires Linux.
    /// At least one limit field is required.
    /// Explicit numeric values must be greater than zero.
    pub fn with_cgroups(mut self, cgroups: CgroupLimits) -> Self {
        self.cgroups = Some(cgroups);
        self
    }

    /// Sets the cgroup v2 parent directory.
    ///
    /// The path must be absolute and identify an existing cgroup v2 directory.
    /// Without this setting, the current process cgroup is used.
    ///
    /// This setting requires [`with_cgroups`](Self::with_cgroups).
    pub fn with_cgroup_parent(mut self, parent: impl Into<PathBuf>) -> Self {
        self.cgroup_parent = Some(parent.into());
        self
    }

    /// Sets Linux process security controls.
    ///
    /// A non-empty policy requires Linux.
    /// [`SeccompPolicy::BlockDangerous`](crate::SeccompPolicy::BlockDangerous) also requires feature `seccomp`.
    pub fn with_security(mut self, security: SecurityConfig) -> Self {
        self.security = Some(security);
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

    /// Validates a task working directory against the configured policy.
    pub(crate) fn check_cwd(&self, cwd: Option<&Path>) -> Result<(), String> {
        self.cwd_policy.check(cwd)
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

    /// Validates and normalizes the complete configuration.
    pub(crate) fn prepare(mut self) -> Result<Self, crate::ExecError> {
        self.cwd_policy.prepare()?;

        if let EnvPolicy::Allowlist(keys) = &self.env_policy {
            for key in keys {
                validate_env_name(key).map_err(InvalidRunnerConfig)?;
            }
        }

        if let Some(cgroups) = &self.cgroups {
            validate_cgroup_limits(cgroups)?;
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
        if let Some(security) = &self.security {
            security.validate()?;
        }

        #[cfg(not(unix))]
        if self
            .rlimits
            .as_ref()
            .is_some_and(|limits| !limits.is_empty())
        {
            return Err(InvalidRunnerConfig(format!(
                "rlimits are not supported on {}",
                std::env::consts::OS
            )));
        }

        #[cfg(not(target_os = "linux"))]
        if self
            .security
            .as_ref()
            .is_some_and(|security| !security.is_empty())
        {
            return Err(InvalidRunnerConfig(format!(
                "process security settings are not supported on {}",
                std::env::consts::OS
            )));
        }

        if self.cgroups.is_some() {
            let explicit_parent = self.cgroup_parent.clone();
            self.cgroup_parent = Some(crate::utils::resolve_cgroup_parent(
                explicit_parent.as_deref(),
            )?);
        } else if self.cgroup_parent.is_some() {
            return Err(InvalidRunnerConfig(
                "cgroup parent is set without cgroup limits".into(),
            ));
        }

        Ok(self)
    }

    /// Returns `true` when cgroup limits are configured.
    pub(crate) fn has_cgroups(&self) -> bool {
        self.cgroups.is_some()
    }

    /// Creates the attempt cgroup and writes its limits.
    ///
    /// This must run before [`apply_to_command`](Self::apply_to_command).
    pub(crate) fn prepare_cgroups(
        &self,
        cgroup_name: &str,
    ) -> Result<Option<PreparedCgroup>, crate::ExecError> {
        if let Some(cgroups) = &self.cgroups {
            trace!(
                "subprocess backend: preparing cgroup: {:?} (group={})",
                cgroups, cgroup_name
            );
            let parent = self
                .cgroup_parent
                .as_deref()
                .expect("validated cgroup config must have a parent");
            crate::utils::prepare_cgroup(parent, cgroup_name, cgroups).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Attaches configured controls to a command.
    ///
    /// The hooks apply rlimits, cgroup membership, and security before `execve`.
    /// The cgroup must already be prepared.
    pub(crate) fn apply_to_command(
        &self,
        cmd: &mut Command,
        prepared_cgroup: Option<PreparedCgroup>,
    ) {
        if let Some(rlimits) = &self.rlimits {
            trace!("subprocess backend: attaching rlimits: {:?}", rlimits);
            attach_rlimits(cmd, rlimits);
        }
        if let Some(prepared) = prepared_cgroup {
            trace!(cgroup = %prepared.path().display(), "attaching cgroup join");
            attach_cgroup(cmd, prepared);
        }
        if let Some(security) = &self.security {
            trace!(
                "subprocess backend: attaching security config: {:?}",
                security
            );
            attach_security(cmd, security);
        }
    }
}

fn validate_cgroup_limits(cgroups: &CgroupLimits) -> Result<(), crate::ExecError> {
    if cgroups.is_empty() {
        return Err(InvalidRunnerConfig(
            "cgroups configuration must contain at least one limit".into(),
        ));
    }
    if let Some(cpu) = &cgroups.cpu {
        if cpu.period == 0 {
            return Err(InvalidRunnerConfig(
                "cgroups.cpu.period cannot be zero".into(),
            ));
        }
        if cpu.quota == Some(0) {
            return Err(InvalidRunnerConfig(
                "cgroups.cpu.quota cannot be zero".into(),
            ));
        }
    }
    if cgroups.memory == Some(0) {
        return Err(InvalidRunnerConfig("cgroups.memory cannot be zero".into()));
    }
    if cgroups.pids == Some(0) {
        return Err(InvalidRunnerConfig("cgroups.pids cannot be zero".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::CpuMax;

    #[test]
    fn invalid_cgroup_limits_are_rejected() {
        let cases = [
            ("empty", CgroupLimits::default(), "at least one limit"),
            (
                "zero CPU period",
                CgroupLimits {
                    cpu: Some(CpuMax {
                        quota: Some(50_000),
                        period: 0,
                    }),
                    ..Default::default()
                },
                "period",
            ),
            (
                "zero CPU quota",
                CgroupLimits {
                    cpu: Some(CpuMax {
                        quota: Some(0),
                        period: 100_000,
                    }),
                    ..Default::default()
                },
                "quota",
            ),
            (
                "zero memory",
                CgroupLimits {
                    memory: Some(0),
                    ..Default::default()
                },
                "memory",
            ),
            (
                "zero process count",
                CgroupLimits {
                    pids: Some(0),
                    ..Default::default()
                },
                "pids",
            ),
        ];

        for (case, limits, expected) in cases {
            let error = validate_cgroup_limits(&limits).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{case}: expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn valid_cpu_limits_are_accepted() {
        for cpu in [
            CpuMax {
                quota: Some(200_000),
                period: 100_000,
            },
            CpuMax {
                quota: None,
                period: 100_000,
            },
        ] {
            let limits = CgroupLimits {
                cpu: Some(cpu),
                ..Default::default()
            };
            assert!(validate_cgroup_limits(&limits).is_ok());
        }
    }

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

    #[test]
    fn cgroup_parent_requires_limits() {
        let config = SubprocessBackendConfig::new().with_cgroup_parent("/tmp");
        let error = config.prepare().unwrap_err().to_string();
        assert!(error.contains("without cgroup limits"), "got: {error}");
    }

    #[test]
    fn max_script_body_bytes_defaults_to_model_const_and_is_configurable() {
        use solti_model::MAX_SCRIPT_BODY_BYTES;

        let default_cfg = SubprocessBackendConfig::new();
        assert_eq!(default_cfg.max_script_body_bytes(), MAX_SCRIPT_BODY_BYTES);

        let custom = SubprocessBackendConfig::new().with_max_script_body_bytes(4096);
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
        let cfg = SubprocessBackendConfig::new();
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
        let cfg = SubprocessBackendConfig::new();
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
