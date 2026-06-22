//! # Backend: OS/kernel subprocess hardening.
//!
//! [`SubprocessBackendConfig`] collects rlimits, cgroup v2, security, and logging settings applied to every subprocess spawned by a runner.

use tokio::process::Command;
use tracing::trace;

use solti_model::MAX_SCRIPT_BODY_BYTES;

use crate::ExecError::InvalidRunnerConfig;
use crate::subprocess::logger::LogConfig;
use crate::utils::{CgroupLimits, RlimitConfig, SecurityConfig};
use crate::utils::{attach_cgroup, attach_rlimits, attach_security};

/// Low-level OS/kernel configuration for subprocess execution.
///
/// Controls resource limits, security policies, and isolation mechanisms.
/// All fields are optional — if not specified, the subprocess inherits parent process settings.
///
/// ## Also
///
/// - [`SubprocessRunner`](super::SubprocessRunner) runner that consumes this config.
/// - [`RlimitConfig`](crate::utils::RlimitConfig) POSIX rlimit knobs.
/// - [`CgroupLimits`](crate::utils::CgroupLimits) cgroup v2 knobs.
/// - [`SecurityConfig`](crate::utils::SecurityConfig) capabilities / seccomp.
/// - [`LogConfig`](super::LogConfig) stdout/stderr log settings.
#[derive(Debug, Clone, Default)]
pub struct SubprocessBackendConfig {
    /// POSIX rlimit-based resource limits.
    rlimits: Option<RlimitConfig>,
    /// Linux cgroup v2 resource limits.
    cgroups: Option<CgroupLimits>,
    /// Security hardening.
    security: Option<SecurityConfig>,
    /// Subprocess output logging configuration.
    logger: LogConfig,
    /// When `true`, confinement that cannot be applied is a hard error rather
    /// than a best-effort warning. See [`with_require_enforcement`](Self::with_require_enforcement).
    require_enforcement: bool,
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

    /// Set security hardening.
    pub fn with_security(mut self, security: SecurityConfig) -> Self {
        self.security = Some(security);
        self
    }

    /// Set logger configuration.
    pub fn with_logger(mut self, config: LogConfig) -> Self {
        self.logger = config;
        self
    }

    /// Require that the configured confinement actually applies (default `false`).
    ///
    /// With the default best-effort policy a sandbox that cannot be applied only warns and runs the child **unconfined**.
    /// When this is `true` and a security/cgroup config is present, confinement fails **closed**:
    /// - on non-Linux it is rejected at config-validation time;
    /// - on Linux the cgroup join and capability drop are forced into fail-on-error mode.
    ///
    /// Use it for security-critical deployments that must never run a job outside its sandbox.
    pub fn with_require_enforcement(mut self, require: bool) -> Self {
        self.require_enforcement = require;
        self
    }

    /// Override the maximum decoded script-body size (default [`MAX_SCRIPT_BODY_BYTES`]).
    ///
    /// Applies to `Script`-mode subprocesses: a body whose decoded size exceeds this is rejected at build time.
    /// A value of `0` is rejected when the runner config is validated.
    pub fn with_max_script_body_bytes(mut self, max: usize) -> Self {
        self.max_script_body_bytes = Some(max);
        self
    }

    /// Effective maximum decoded script-body size (falls back to [`MAX_SCRIPT_BODY_BYTES`]).
    pub(crate) fn max_script_body_bytes(&self) -> usize {
        self.max_script_body_bytes.unwrap_or(MAX_SCRIPT_BODY_BYTES)
    }

    /// Cgroup limits with `fail_on_error` forced on when enforcement is required.
    fn effective_cgroups(&self, cgroups: &CgroupLimits) -> CgroupLimits {
        let mut c = cgroups.clone();
        if self.require_enforcement {
            c.fail_on_error = true;
        }
        c
    }

    /// Get log configuration.
    pub(crate) fn log_config(&self) -> &LogConfig {
        &self.logger
    }

    /// Check if any backend features are configured.
    pub(crate) fn is_empty(&self) -> bool {
        self.rlimits.is_none() && self.cgroups.is_none() && self.security.is_none()
    }

    /// Validate the configuration.
    pub(crate) fn validate(&self) -> Result<(), crate::ExecError> {
        if let Some(cgroups) = &self.cgroups {
            if let Some(cpu) = &cgroups.cpu {
                if cpu.period == 0 {
                    return Err(InvalidRunnerConfig(
                        "cgroups.cpu.period cannot be zero".into(),
                    ));
                }
                if let Some(q) = cpu.quota
                    && q == 0
                {
                    return Err(InvalidRunnerConfig(
                        "cgroups.cpu.quota cannot be zero (process would get no CPU)".into(),
                    ));
                }
                if let Some(q) = cpu.quota
                    && q > cpu.period
                {
                    return Err(InvalidRunnerConfig(
                        "cgroups.cpu.quota exceeds period (>100% of one core)".into(),
                    ));
                }
            }
            if let Some(mem) = cgroups.memory
                && mem == 0
            {
                return Err(InvalidRunnerConfig("cgroups.memory cannot be zero".into()));
            }
            if let Some(pids) = cgroups.pids
                && pids == 0
            {
                return Err(InvalidRunnerConfig("cgroups.pids cannot be zero".into()));
            }
        }
        if let Some(rlimits) = &self.rlimits
            && let Some(fsize) = rlimits.max_file_size_bytes
            && fsize == 0
        {
            return Err(InvalidRunnerConfig(
                "rlimits.max_file_size_bytes cannot be zero".into(),
            ));
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
        if self.max_script_body_bytes == Some(0) {
            return Err(InvalidRunnerConfig(
                "max_script_body_bytes cannot be zero (all scripts would be rejected)".into(),
            ));
        }
        if let Some(security) = &self.security {
            security.validate()?;
        }
        #[cfg(not(target_os = "linux"))]
        if self.require_enforcement && !self.is_empty() {
            return Err(InvalidRunnerConfig(format!(
                "require_enforcement is set but OS={} cannot enforce cgroup/security confinement",
                std::env::consts::OS
            )));
        }
        Ok(())
    }

    /// Check if cgroup limits are configured.
    pub(crate) fn has_cgroups(&self) -> bool {
        self.cgroups.is_some()
    }

    /// Prepare cgroup directory and write limit files (before spawn).
    ///
    /// Must be called before `apply_to_command`. Returns `Ok(true)` if a cgroup was created successfully.
    /// Runs in normal async context (safe to use std::fs).
    pub(crate) fn prepare_cgroups(&self, cgroup_name: &str) -> Result<bool, crate::ExecError> {
        if let Some(cgroups) = &self.cgroups {
            trace!(
                "subprocess backend: preparing cgroup: {:?} (group={})",
                cgroups, cgroup_name
            );
            crate::utils::prepare_cgroup(cgroup_name, &self.effective_cgroups(cgroups))
        } else {
            Ok(false)
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
        cgroup_name: &str,
    ) -> Result<(), crate::ExecError> {
        if self.is_empty() {
            trace!("subprocess backend: nothing to apply (empty config)");
            return Ok(());
        }

        if let Some(rlimits) = &self.rlimits {
            trace!("subprocess backend: attaching rlimits: {:?}", rlimits);
            attach_rlimits(cmd, rlimits);
        }
        if let Some(cgroups) = &self.cgroups {
            trace!(
                "subprocess backend: attaching cgroup join hook (group={})",
                cgroup_name
            );
            attach_cgroup(cmd, cgroup_name, &self.effective_cgroups(cgroups))?;
        }
        if let Some(security) = &self.security {
            trace!(
                "subprocess backend: attaching security config: {:?}",
                security
            );
            let security = if self.require_enforcement {
                let mut s = security.clone();
                s.fail_on_cap_error = true;
                s
            } else {
                security.clone()
            };
            attach_security(cmd, &security);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::CpuMax;

    #[test]
    fn valid_cpu_config_passes() {
        let cfg = SubprocessBackendConfig::new().with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(50_000),
                period: 100_000,
            }),
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn cpu_period_zero_rejected() {
        let cfg = SubprocessBackendConfig::new().with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(50_000),
                period: 0,
            }),
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("period"), "expected period error, got: {err}");
    }

    #[test]
    fn cpu_quota_zero_rejected() {
        let cfg = SubprocessBackendConfig::new().with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(0),
                period: 100_000,
            }),
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("quota"), "expected quota error, got: {err}");
    }

    #[test]
    fn cpu_quota_exceeds_period_rejected() {
        let cfg = SubprocessBackendConfig::new().with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(200_000),
                period: 100_000,
            }),
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("exceeds period"),
            "expected exceeds error, got: {err}"
        );
    }

    #[test]
    fn cpu_unlimited_quota_passes() {
        let cfg = SubprocessBackendConfig::new().with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: None,
                period: 100_000,
            }),
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn max_line_bytes_zero_rejected() {
        let cfg = SubprocessBackendConfig::new().with_logger(LogConfig {
            max_line_bytes: 0,
            ..LogConfig::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("max_line_bytes"), "got: {err}");
    }

    #[test]
    fn keep_caps_without_drop_all_caps_rejected() {
        use crate::utils::{LinuxCapability, SecurityConfig};
        let cfg = SubprocessBackendConfig::new().with_security(SecurityConfig {
            drop_all_caps: false,
            keep_caps: vec![LinuxCapability::NetBindService],
            ..Default::default()
        });
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("keep_caps") && err.contains("drop_all_caps"),
            "got: {err}"
        );
    }

    #[test]
    fn keep_caps_with_drop_all_caps_passes() {
        use crate::utils::{LinuxCapability, SecurityConfig};
        let cfg = SubprocessBackendConfig::new().with_security(SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetBindService],
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn require_enforcement_fails_closed_on_non_linux() {
        use crate::utils::SecurityConfig;
        let cfg = SubprocessBackendConfig::new()
            .with_security(SecurityConfig {
                no_new_privs: true,
                ..Default::default()
            })
            .with_require_enforcement(true);
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("require_enforcement"), "got: {err}");
    }

    #[test]
    fn require_enforcement_with_empty_config_is_ok() {
        let cfg = SubprocessBackendConfig::new().with_require_enforcement(true);
        assert!(cfg.validate().is_ok());
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
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("max_script_body_bytes"), "got: {err}");
    }
}
