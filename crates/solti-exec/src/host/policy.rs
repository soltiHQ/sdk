//! # Host process policy
//!
//! [`HostProcessPolicy`] groups controls applied to one host process.
//! It does not define workload transport or supervision.
//!
//! ## Flow
//!
//! ```text
//! HostProcessPolicy
//!        │ backend construction
//!        ▼
//! validate platform and values
//!        ▼
//! PreparedHostProcessPolicy
//!        │ each attempt
//!        ├── prepare attempt resources
//!        ├── attach child hooks
//!        └── return cleanup guard
//! ```

use std::{
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(feature = "host-process")]
use std::process::Command;
#[cfg(feature = "host-process")]
use tracing::{trace, warn};

use super::{CgroupLimits, RlimitConfig, SecurityConfig};
#[cfg(feature = "host-process")]
use super::{
    HostProcessError, PreparedCgroup, attach_cgroup, attach_rlimits, attach_security,
    cleanup_cgroup, prepare_cgroup, resolve_cgroup_parent,
};

/// Declarative controls for a process started on the host.
///
/// The default policy enables no control.
/// Non-empty rlimits require Unix.
/// Cgroups and security controls require Linux.
///
/// This policy describes host process hardening.
/// It does not guarantee complete isolation from the agent or other workloads.
///
/// ## Example
///
/// ```rust
/// use solti_exec::host::{HostProcessPolicy, RlimitConfig};
///
/// let policy = HostProcessPolicy::new().with_rlimits(RlimitConfig {
///     max_open_files: Some(1024),
///     ..Default::default()
/// });
///
/// assert!(!policy.is_empty());
/// ```
#[derive(Debug, Clone, Default)]
pub struct HostProcessPolicy {
    /// POSIX rlimit-based resource limits.
    rlimits: Option<RlimitConfig>,
    /// Linux cgroup v2 resource limits.
    cgroups: Option<CgroupLimits>,
    /// Parent directory for per-attempt cgroups.
    cgroup_parent: Option<PathBuf>,
    /// Linux process security controls.
    security: Option<SecurityConfig>,
}

impl HostProcessPolicy {
    /// Creates an empty host process policy.
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
    /// Host process enforcement requires feature `seccomp` to apply
    /// [`super::SeccompPolicy::BlockDangerous`].
    pub fn with_security(mut self, security: SecurityConfig) -> Self {
        self.security = Some(security);
        self
    }

    /// Returns configured POSIX process limits.
    pub fn rlimits(&self) -> Option<&RlimitConfig> {
        self.rlimits.as_ref()
    }

    /// Returns configured Linux cgroup v2 limits.
    pub fn cgroups(&self) -> Option<&CgroupLimits> {
        self.cgroups.as_ref()
    }

    /// Returns the explicit cgroup v2 parent.
    ///
    /// `None` lets the backend select its default parent.
    pub fn cgroup_parent(&self) -> Option<&Path> {
        self.cgroup_parent.as_deref()
    }

    /// Returns configured Linux process security controls.
    pub fn security(&self) -> Option<&SecurityConfig> {
        self.security.as_ref()
    }

    /// Returns `true` when no host process control is configured.
    pub fn is_empty(&self) -> bool {
        self.rlimits.as_ref().is_none_or(RlimitConfig::is_empty)
            && self.cgroups.is_none()
            && self.cgroup_parent.is_none()
            && self.security.as_ref().is_none_or(SecurityConfig::is_empty)
    }

    /// Validates and prepares the policy for one backend.
    ///
    /// Platform support and cgroup parent resolution are checked here.
    ///
    /// # Errors
    ///
    /// Returns [`HostProcessError::InvalidConfig`] for invalid or unsupported controls.
    /// Returns [`HostProcessError::Io`] when current cgroup discovery fails.
    #[cfg(feature = "host-process")]
    pub fn prepare(self) -> Result<PreparedHostProcessPolicy, HostProcessError> {
        if let Some(cgroups) = &self.cgroups {
            validate_cgroup_limits(cgroups)?;
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
            return Err(HostProcessError::InvalidConfig(format!(
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
            return Err(HostProcessError::InvalidConfig(format!(
                "process security settings are not supported on {}",
                std::env::consts::OS
            )));
        }

        let cgroup_parent = match (self.cgroups.as_ref(), self.cgroup_parent.as_deref()) {
            (Some(_), explicit) => Some(resolve_cgroup_parent(explicit)?),
            (None, Some(_)) => {
                return Err(HostProcessError::InvalidConfig(
                    "cgroup parent is set without cgroup limits".into(),
                ));
            }
            (None, None) => None,
        };

        Ok(PreparedHostProcessPolicy {
            controls: Arc::new(HostProcessControls {
                rlimits: self.rlimits,
                security: self.security,
            }),
            cgroups: self.cgroups,
            cgroup_parent,
        })
    }
}

#[derive(Debug)]
struct HostProcessControls {
    rlimits: Option<RlimitConfig>,
    security: Option<SecurityConfig>,
}

/// Host process policy prepared during backend construction.
#[cfg(feature = "host-process")]
#[derive(Debug)]
pub struct PreparedHostProcessPolicy {
    controls: Arc<HostProcessControls>,
    cgroups: Option<CgroupLimits>,
    cgroup_parent: Option<PathBuf>,
}

/// Attempt resources prepared before process creation.
///
/// This token proves that all configured pre-spawn resources were prepared.
/// Call [`apply_to_command`](Self::apply_to_command) to attach its policy.
#[must_use = "prepared host process resources must be applied or dropped"]
pub struct PreparedHostProcessAttempt {
    controls: Arc<HostProcessControls>,
    cgroup: Option<PreparedCgroup>,
}

/// Owns resources attached to one host process attempt.
///
/// Keep this guard until the complete process scope has stopped.
/// Call [`cleanup`](Self::cleanup) after that point.
/// Drop performs one best-effort cleanup attempt.
#[must_use = "the host process guard must be held until the process scope stops"]
pub struct HostProcessGuard {
    cgroup_path: Option<PathBuf>,
}

impl HostProcessGuard {
    /// Returns the attempt cgroup directory.
    ///
    /// `None` means that the policy has no cgroup limits.
    pub fn cgroup_path(&self) -> Option<&Path> {
        self.cgroup_path.as_deref()
    }

    /// Removes owned resources after the process scope has stopped.
    ///
    /// This method is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when an owned cgroup cannot be removed.
    pub fn cleanup(&mut self) -> io::Result<()> {
        let Some(path) = self.cgroup_path.as_deref() else {
            return Ok(());
        };
        match cleanup_cgroup(path) {
            Ok(()) => {
                self.cgroup_path = None;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.cgroup_path = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(cgroup_path: PathBuf) -> Self {
        Self {
            cgroup_path: Some(cgroup_path),
        }
    }
}

impl Drop for HostProcessGuard {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup()
            && let Some(path) = self.cgroup_path.as_deref()
        {
            warn!(
                cgroup = %path.display(),
                error = %error,
                "failed to clean up host process cgroup",
            );
        }
    }
}

#[cfg(feature = "host-process")]
impl PreparedHostProcessPolicy {
    /// Returns `true` when cgroup limits are configured.
    pub fn has_cgroups(&self) -> bool {
        self.cgroups.is_some()
    }

    /// Prepares resources for one process attempt.
    ///
    /// `cgroup_name` is required when cgroup limits are configured.
    /// It must contain one normal path component.
    ///
    /// # Errors
    ///
    /// Returns [`HostProcessError::InvalidConfig`] for a missing or unsafe cgroup name.
    /// Returns [`HostProcessError::Io`] when the cgroup cannot be created or configured.
    pub fn prepare_attempt(
        &self,
        cgroup_name: Option<&str>,
    ) -> Result<PreparedHostProcessAttempt, HostProcessError> {
        let cgroup = match (&self.cgroups, cgroup_name) {
            (Some(cgroups), Some(name)) => {
                validate_cgroup_name(name)?;
                trace!(?cgroups, group = name, "preparing host process cgroup");
                let parent = self
                    .cgroup_parent
                    .as_deref()
                    .expect("prepared cgroup policy must have a parent");
                Some(prepare_cgroup(parent, name, cgroups)?)
            }
            (Some(_), None) => {
                return Err(HostProcessError::InvalidConfig(
                    "cgroup name is required when cgroup limits are configured".into(),
                ));
            }
            (None, _) => None,
        };

        Ok(PreparedHostProcessAttempt {
            controls: Arc::clone(&self.controls),
            cgroup,
        })
    }
}

impl PreparedHostProcessAttempt {
    /// Attaches enabled controls to a process command.
    ///
    /// This token is consumed.
    /// The returned guard owns resources that require post-process cleanup.
    pub fn apply_to_command(self, command: &mut Command) -> HostProcessGuard {
        let Self { controls, cgroup } = self;
        let guard = HostProcessGuard {
            cgroup_path: cgroup
                .as_ref()
                .map(|prepared| prepared.path().to_path_buf()),
        };

        if let Some(rlimits) = &controls.rlimits {
            trace!(?rlimits, "attaching host process rlimits");
            attach_rlimits(command, rlimits);
        }
        if let Some(prepared) = cgroup {
            trace!(
                cgroup = %prepared.path().display(),
                "attaching host process cgroup",
            );
            attach_cgroup(command, prepared);
        }
        if let Some(security) = &controls.security {
            trace!(?security, "attaching host process security controls");
            attach_security(command, security);
        }

        guard
    }
}

fn validate_cgroup_name(name: &str) -> Result<(), HostProcessError> {
    let mut components = Path::new(name).components();
    let valid =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(HostProcessError::InvalidConfig(format!(
            "cgroup name must be one normal path component: {name:?}"
        )))
    }
}

#[cfg(feature = "host-process")]
fn validate_cgroup_limits(cgroups: &CgroupLimits) -> Result<(), HostProcessError> {
    if cgroups.is_empty() {
        return Err(HostProcessError::InvalidConfig(
            "cgroups configuration must contain at least one limit".into(),
        ));
    }
    if let Some(cpu) = &cgroups.cpu {
        if cpu.period == 0 {
            return Err(HostProcessError::InvalidConfig(
                "cgroups.cpu.period cannot be zero".into(),
            ));
        }
        if cpu.quota == Some(0) {
            return Err(HostProcessError::InvalidConfig(
                "cgroups.cpu.quota cannot be zero".into(),
            ));
        }
    }
    if cgroups.memory == Some(0) {
        return Err(HostProcessError::InvalidConfig(
            "cgroups.memory cannot be zero".into(),
        ));
    }
    if cgroups.pids == Some(0) {
        return Err(HostProcessError::InvalidConfig(
            "cgroups.pids cannot be zero".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "host-process")]
    use crate::host::CpuMax;

    #[test]
    fn default_policy_is_empty() {
        assert!(HostProcessPolicy::new().is_empty());
    }

    #[test]
    fn policy_components_are_available_to_backends() {
        let policy = HostProcessPolicy::new()
            .with_rlimits(RlimitConfig {
                max_open_files: Some(128),
                ..Default::default()
            })
            .with_cgroups(CgroupLimits {
                memory: Some(1024),
                ..Default::default()
            })
            .with_cgroup_parent("/sys/fs/cgroup/solti")
            .with_security(SecurityConfig {
                no_new_privs: true,
                ..Default::default()
            });

        assert_eq!(
            policy.rlimits().and_then(|limits| limits.max_open_files),
            Some(128)
        );
        assert_eq!(
            policy.cgroups().and_then(|limits| limits.memory),
            Some(1024)
        );
        assert_eq!(
            policy.cgroup_parent(),
            Some(Path::new("/sys/fs/cgroup/solti"))
        );
        assert!(policy.security().is_some_and(|policy| policy.no_new_privs));
    }

    #[test]
    #[cfg(feature = "host-process")]
    fn cgroup_limits_require_one_non_zero_control() {
        let cases = [
            (CgroupLimits::default(), "at least one limit"),
            (
                CgroupLimits {
                    cpu: Some(CpuMax {
                        quota: Some(1),
                        period: 0,
                    }),
                    ..Default::default()
                },
                "period cannot be zero",
            ),
            (
                CgroupLimits {
                    cpu: Some(CpuMax {
                        quota: Some(0),
                        period: 100_000,
                    }),
                    ..Default::default()
                },
                "quota cannot be zero",
            ),
            (
                CgroupLimits {
                    memory: Some(0),
                    ..Default::default()
                },
                "memory cannot be zero",
            ),
            (
                CgroupLimits {
                    pids: Some(0),
                    ..Default::default()
                },
                "pids cannot be zero",
            ),
        ];

        for (limits, expected) in cases {
            let error = validate_cgroup_limits(&limits).unwrap_err().to_string();
            assert!(error.contains(expected), "got: {error}");
        }
    }

    #[test]
    #[cfg(feature = "host-process")]
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

    #[cfg(unix)]
    #[test]
    fn prepared_policy_applies_rlimits_to_a_process() {
        let requested = crate::host::reduced_nofile_soft_limit_for_test();
        let policy = HostProcessPolicy::new()
            .with_rlimits(RlimitConfig {
                max_open_files: Some(requested),
                ..Default::default()
            })
            .prepare()
            .unwrap();
        let attempt = policy.prepare_attempt(None).unwrap();
        let mut command = Command::new("sh");
        command.arg("-c").arg("ulimit -n");

        let _guard = attempt.apply_to_command(&mut command);
        let output = command.output().unwrap();

        assert!(output.status.success());
        let actual = std::str::from_utf8(&output.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap();
        assert_eq!(actual, requested);
    }

    #[test]
    fn cgroup_name_must_be_one_normal_component() {
        for valid in ["attempt", "runner-slot-2a-3e8"] {
            assert!(validate_cgroup_name(valid).is_ok());
        }
        for invalid in ["", ".", "..", "../attempt", "parent/attempt", "/attempt"] {
            assert!(validate_cgroup_name(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn host_process_guard_owns_cgroup_cleanup() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let mut guard = HostProcessGuard::for_test(path.clone());

        assert_eq!(guard.cgroup_path(), Some(path.as_path()));
        guard.cleanup().unwrap();
        guard.cleanup().unwrap();

        assert!(guard.cgroup_path().is_none());
        assert!(!path.exists());
    }

    #[test]
    #[cfg(feature = "host-process")]
    fn cgroup_parent_requires_limits() {
        let error = HostProcessPolicy::new()
            .with_cgroup_parent("/tmp")
            .prepare()
            .unwrap_err()
            .to_string();
        assert!(error.contains("without cgroup limits"), "got: {error}");
    }
}
