//! # Backend: OS/kernel subprocess hardening.
//!
//! [`SubprocessBackendConfig`] collects rlimits, cgroup v2, security, and logging settings applied to every subprocess spawned by a runner.

use tokio::process::Command;
use tracing::trace;

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
            crate::utils::prepare_cgroup(cgroup_name, cgroups)
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
            attach_cgroup(cmd, cgroup_name, cgroups)?;
        }
        if let Some(security) = &self.security {
            trace!(
                "subprocess backend: attaching security config: {:?}",
                security
            );
            attach_security(cmd, security);
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
}
