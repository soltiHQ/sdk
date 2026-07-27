//! Runner-wide subprocess settings.
//!
//! [`SubprocessBackendConfig`] collects rlimits, cgroup v2, security, logging,
//! and environment settings applied to every subprocess spawned by a runner.

use std::path::{Path, PathBuf};

use tokio::process::Command;
use tracing::trace;

use solti_model::MAX_SCRIPT_BODY_BYTES;

use crate::ExecError::InvalidRunnerConfig;
use crate::subprocess::logger::LogConfig;
use crate::utils::{CgroupLimits, PreparedCgroup, RlimitConfig, SecurityConfig};
use crate::utils::{attach_cgroup, attach_rlimits, attach_security};

/// Minimal `PATH` injected when the environment is cleared and the task did not
/// set its own. Without it, a bare command name (`echo`) would fail to resolve.
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
#[derive(Debug, Clone, Default)]
pub enum EnvPolicy {
    /// Inherit the process environment, then apply task variables.
    Inherit,
    /// Use task variables and a safe default `PATH` only.
    #[default]
    Clear,
    /// Pass only the named process variables, task variables, and `PATH`.
    Allowlist(Vec<String>),
}

/// Where a task is allowed to set its working directory.
///
/// [`Roots`](Self::Roots) checks the starting directory at build time.
/// It does not restrict files the process can open after it starts.
#[derive(Debug, Clone, Default)]
pub enum CwdPolicy {
    /// Allow any `cwd` in the task spec.
    #[default]
    Unrestricted,
    /// Require `cwd` to resolve inside one of these directories.
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

    /// Check a task-provided `cwd` against the policy.
    ///
    /// Under [`Unrestricted`](Self::Unrestricted) anything is allowed. Under
    /// [`Roots`](Self::Roots) the `cwd` is required and is canonicalized first,
    /// which resolves symlinks and `..` so a crafted path cannot traverse out
    /// of an allowed root at validation time.
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
    /// How the child environment is built. Default [`EnvPolicy::Clear`].
    env_policy: EnvPolicy,
    /// Where a task may set its working directory. Default [`CwdPolicy::Unrestricted`].
    cwd_policy: CwdPolicy,
    /// Subprocess output logging configuration.
    logger: LogConfig,
    /// Maximum decoded script-body size for `Script`-mode subprocesses.
    /// `None` uses the model default [`MAX_SCRIPT_BODY_BYTES`].
    max_script_body_bytes: Option<usize>,
}

impl SubprocessBackendConfig {
    /// Create an empty backend config (no limits).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set rlimits.
    pub fn with_rlimits(mut self, rlimits: RlimitConfig) -> Self {
        self.rlimits = Some(rlimits);
        self
    }

    /// Set cgroup limits.
    pub fn with_cgroups(mut self, cgroups: CgroupLimits) -> Self {
        self.cgroups = Some(cgroups);
        self
    }

    /// Set the cgroup v2 parent directory.
    ///
    /// When omitted, the runner creates child groups under the process's current
    /// delegated cgroup.
    pub fn with_cgroup_parent(mut self, parent: impl Into<PathBuf>) -> Self {
        self.cgroup_parent = Some(parent.into());
        self
    }

    /// Set security hardening.
    pub fn with_security(mut self, security: SecurityConfig) -> Self {
        self.security = Some(security);
        self
    }

    /// Set the environment policy for spawned children.
    pub fn with_env_policy(mut self, policy: EnvPolicy) -> Self {
        self.env_policy = policy;
        self
    }

    /// Restrict where a task may set its working directory (default
    /// [`CwdPolicy::Unrestricted`]).
    ///
    /// Use [`CwdPolicy::Roots`] to confine tasks to a set of directories; a spec
    /// whose `cwd` escapes them is rejected at build time.
    pub fn with_cwd_policy(mut self, policy: CwdPolicy) -> Self {
        self.cwd_policy = policy;
        self
    }

    /// Validate a task-provided `cwd` against the configured [`CwdPolicy`].
    pub(crate) fn check_cwd(&self, cwd: Option<&Path>) -> Result<(), String> {
        self.cwd_policy.check(cwd)
    }

    /// Set logger configuration.
    pub fn with_logger(mut self, config: LogConfig) -> Self {
        self.logger = config;
        self
    }

    /// Tighten the maximum decoded script-body size (default [`MAX_SCRIPT_BODY_BYTES`]).
    ///
    /// Applies to `Script`-mode subprocesses: a body whose decoded size exceeds this is rejected at build time.
    /// Zero and values above the hard 2 MiB model limit are rejected when the
    /// runner config is validated.
    pub fn with_max_script_body_bytes(mut self, max: usize) -> Self {
        self.max_script_body_bytes = Some(max);
        self
    }

    /// Effective maximum decoded script-body size (falls back to [`MAX_SCRIPT_BODY_BYTES`]).
    pub(crate) fn max_script_body_bytes(&self) -> usize {
        self.max_script_body_bytes.unwrap_or(MAX_SCRIPT_BODY_BYTES)
    }

    /// Environment policy applied to a child.
    pub(crate) fn env_policy(&self) -> &EnvPolicy {
        &self.env_policy
    }

    /// Get log configuration.
    pub(crate) fn log_config(&self) -> &LogConfig {
        &self.logger
    }

    /// Validate and normalize the configuration once at runner construction.
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

    /// Check if cgroup limits are configured.
    pub(crate) fn has_cgroups(&self) -> bool {
        self.cgroups.is_some()
    }

    /// Prepare cgroup directory and write limit files (before spawn).
    ///
    /// Must be called before `apply_to_command`.
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

    /// Apply all configured backend features to a `tokio::process::Command`.
    ///
    /// This method mutates the command by attaching pre_exec hooks for:
    /// - cgroups (join only — directory must be created via [`prepare_cgroups`] first)
    /// - security policies
    /// - rlimits
    ///
    /// Call this immediately before spawning the subprocess.
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
    use crate::utils::{CpuMax, LinuxCapability};

    #[test]
    fn cpu_period_zero_rejected() {
        let limits = CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(50_000),
                period: 0,
            }),
            ..Default::default()
        };
        let err = validate_cgroup_limits(&limits).unwrap_err().to_string();
        assert!(err.contains("period"), "expected period error, got: {err}");
    }

    #[test]
    fn cpu_quota_zero_rejected() {
        let limits = CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(0),
                period: 100_000,
            }),
            ..Default::default()
        };
        let err = validate_cgroup_limits(&limits).unwrap_err().to_string();
        assert!(err.contains("quota"), "expected quota error, got: {err}");
    }

    #[test]
    fn cpu_quota_may_exceed_period() {
        let limits = CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(200_000),
                period: 100_000,
            }),
            ..Default::default()
        };
        assert!(validate_cgroup_limits(&limits).is_ok());
    }

    #[test]
    fn cpu_unlimited_quota_is_valid() {
        let limits = CgroupLimits {
            cpu: Some(CpuMax {
                quota: None,
                period: 100_000,
            }),
            ..Default::default()
        };
        assert!(validate_cgroup_limits(&limits).is_ok());
    }

    #[test]
    fn max_line_bytes_zero_rejected() {
        let cfg = SubprocessBackendConfig::new().with_logger(LogConfig {
            max_line_bytes: 0,
            ..LogConfig::default()
        });
        let err = cfg.prepare().unwrap_err().to_string();
        assert!(err.contains("max_line_bytes"), "got: {err}");
    }

    #[test]
    fn keep_caps_without_drop_all_caps_rejected() {
        let security = SecurityConfig {
            drop_all_caps: false,
            keep_caps: vec![LinuxCapability::NetBindService],
            ..Default::default()
        };
        let err = security.validate().unwrap_err().to_string();
        assert!(
            err.contains("keep_caps") && err.contains("drop_all_caps"),
            "got: {err}"
        );
    }

    #[test]
    fn keep_caps_with_drop_all_caps_is_valid() {
        let security = SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetBindService],
            ..Default::default()
        };
        assert!(security.validate().is_ok());
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
    fn max_script_body_bytes_zero_rejected() {
        let cfg = SubprocessBackendConfig::new().with_max_script_body_bytes(0);
        let err = cfg.prepare().unwrap_err().to_string();
        assert!(err.contains("max_script_body_bytes"), "got: {err}");
    }

    #[test]
    fn max_script_body_bytes_above_hard_limit_rejected() {
        let cfg =
            SubprocessBackendConfig::new().with_max_script_body_bytes(MAX_SCRIPT_BODY_BYTES + 1);
        let err = cfg.prepare().unwrap_err().to_string();
        assert!(err.contains("1..="), "got: {err}");
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
