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
//!        └── return attempt process domain
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

use super::{
    CgroupDomain, CgroupLimits, DomainTermination, ProcessConfig, RlimitConfig, SecurityConfig,
};
#[cfg(feature = "host-process")]
use super::{
    CgroupPrepareFailure, HostProcessError, PreparedCgroup, PreparedCgroupParent,
    PreparedProcessConfig, PreparedRlimits, attach_cgroup, attach_process_config, attach_rlimits,
    attach_security, prepare_cgroup_owned, resolve_cgroup_parent,
};
use crate::isolation::validate_cgroup_limits;

/// Declarative controls for a process started on the host.
///
/// The default policy enables no control.
/// Non-empty process state and rlimits require Unix.
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
    /// Unix process state.
    process: Option<ProcessConfig>,
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

    /// Sets Unix process state.
    ///
    /// A non-empty configuration requires Unix.
    pub fn with_process_config(mut self, process: ProcessConfig) -> Self {
        self.process = Some(process);
        self
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
    /// Workloads must not have write access to the selected parent.
    ///
    /// This setting requires [`with_cgroups`](Self::with_cgroups).
    pub fn with_cgroup_parent(mut self, parent: impl Into<PathBuf>) -> Self {
        self.cgroup_parent = Some(parent.into());
        self
    }

    /// Sets Linux process security controls.
    ///
    /// A non-empty policy requires Linux.
    /// Host process enforcement requires feature `seccomp` to apply [`super::SeccompPolicy::DenyHostControl`].
    pub fn with_security(mut self, security: SecurityConfig) -> Self {
        self.security = Some(security);
        self
    }

    /// Returns configured Unix process state.
    pub fn process_config(&self) -> Option<&ProcessConfig> {
        self.process.as_ref()
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
        self.process.as_ref().is_none_or(ProcessConfig::is_empty)
            && self.rlimits.as_ref().is_none_or(RlimitConfig::is_empty)
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
    /// Returns [`HostProcessError::Io`] when host resource preparation fails.
    #[cfg(feature = "host-process")]
    pub fn prepare(self) -> Result<PreparedHostProcessPolicy, HostProcessError> {
        if let Some(cgroups) = &self.cgroups {
            validate_cgroup_limits(cgroups).map_err(HostProcessError::InvalidConfig)?;
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

        let process = self
            .process
            .as_ref()
            .filter(|process| !process.is_empty())
            .map(ProcessConfig::prepare)
            .transpose()?;

        let rlimits = self
            .rlimits
            .as_ref()
            .map(RlimitConfig::prepare)
            .transpose()?;

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
                process,
                rlimits,
                security: self.security,
            }),
            cgroups: self.cgroups,
            cgroup_parent,
        })
    }
}

#[derive(Debug)]
struct HostProcessControls {
    process: Option<PreparedProcessConfig>,
    rlimits: Option<PreparedRlimits>,
    security: Option<SecurityConfig>,
}

/// Host process policy prepared during backend construction.
#[cfg(feature = "host-process")]
#[derive(Debug)]
pub struct PreparedHostProcessPolicy {
    controls: Arc<HostProcessControls>,
    cgroups: Option<CgroupLimits>,
    cgroup_parent: Option<PreparedCgroupParent>,
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

/// Internal attempt preparation failure with residual cleanup ownership.
#[cfg(feature = "host-process")]
#[derive(Debug)]
pub(crate) enum AttemptPrepareFailure {
    /// Preparation failed after complete rollback.
    Clean(HostProcessError),
    /// Preparation failed and rollback left owned cgroup state.
    Residual {
        /// Original preparation error returned to the caller.
        error: HostProcessError,
        /// Safely pinned cleanup ownership, or `None` when no safe identity is available.
        cleanup: Option<AttemptProcessDomain>,
    },
}

#[cfg(feature = "host-process")]
impl AttemptPrepareFailure {
    pub(crate) fn into_error(self) -> HostProcessError {
        match self {
            Self::Clean(error) | Self::Residual { error, .. } => error,
        }
    }
}

/// Owns cgroup resources attached to one host process attempt.
///
/// Keep this value until the attached process scope has stopped.
/// Call [`terminate_tree`](Self::terminate_tree) to request cgroup subtree termination.
/// Call [`cleanup`](Self::cleanup) after that point.
/// Drop performs synchronous termination and one best-effort cleanup attempt.
#[must_use = "the attempt process domain must be held until the process scope stops"]
#[derive(Debug)]
pub struct AttemptProcessDomain {
    cgroup: Option<CgroupDomain>,
}

impl AttemptProcessDomain {
    /// Returns the attempt cgroup directory.
    ///
    /// `None` means that the policy has no cgroup limits.
    pub fn cgroup_path(&self) -> Option<&Path> {
        self.cgroup.as_ref().and_then(CgroupDomain::path)
    }

    /// Requests termination of the configured cgroup subtree.
    ///
    /// This method is idempotent.
    /// A cgroup-backed domain writes once to its pinned `cgroup.kill` descriptor.
    /// A successful result confirms the request, not process exit.
    /// A domain without `cgroup.kill` returns [`DomainTermination::Unavailable`].
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the termination request fails.
    pub fn terminate_tree(&mut self) -> io::Result<DomainTermination> {
        match self.cgroup.as_mut() {
            Some(cgroup) => cgroup.terminate_tree(),
            None => Ok(DomainTermination::Unavailable),
        }
    }

    /// Returns whether the configured cgroup contains live processes.
    ///
    /// `None` means that the policy has no cgroup.
    /// The result includes descendant cgroups.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `cgroup.events` cannot be read or parsed.
    pub fn cgroup_populated(&self) -> io::Result<Option<bool>> {
        match self.cgroup.as_ref() {
            Some(cgroup) => cgroup.is_populated(),
            None => Ok(None),
        }
    }

    /// Removes owned resources after the process scope has stopped.
    ///
    /// This method is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the cgroup is populated, no longer identifies the owned directory, or cannot be removed.
    pub fn cleanup(&mut self) -> io::Result<()> {
        if let Some(cgroup) = self.cgroup.as_mut() {
            cgroup.cleanup()?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(cgroup_path: PathBuf) -> Self {
        Self {
            cgroup: Some(CgroupDomain::for_test(cgroup_path)),
        }
    }
}

impl Drop for AttemptProcessDomain {
    fn drop(&mut self) {
        let termination = self.terminate_tree();
        #[cfg(feature = "host-process")]
        if let Err(error) = termination {
            warn!(
                event = "host_process.termination_failed",
                cgroup = ?self.cgroup_path(),
                error = %error,
                "failed to terminate host process domain",
            );
        }
        #[cfg(not(feature = "host-process"))]
        let _ = termination;

        let cleanup = self.cleanup();
        #[cfg(feature = "host-process")]
        if let Err(error) = cleanup {
            warn!(
                event = "host_process.cleanup_failed",
                cgroup = ?self.cgroup_path(),
                error = %error,
                "failed to clean up host process cgroup",
            );
        }
        #[cfg(not(feature = "host-process"))]
        let _ = cleanup;
    }
}

#[cfg(feature = "host-process")]
impl PreparedHostProcessPolicy {
    /// Returns `true` when subprocesses create a new Unix session.
    #[cfg(feature = "subprocess")]
    pub(crate) fn starts_new_session(&self) -> bool {
        self.controls
            .process
            .as_ref()
            .is_some_and(PreparedProcessConfig::starts_new_session)
    }

    /// Returns resolved POSIX process limit ceilings.
    ///
    /// Requested values above inherited hard limits are clamped here.
    pub fn rlimits(&self) -> Option<&PreparedRlimits> {
        self.controls.rlimits.as_ref()
    }

    /// Returns `true` when cgroup limits are configured.
    pub fn has_cgroups(&self) -> bool {
        self.cgroups.is_some()
    }

    /// Prepares resources for one process attempt.
    ///
    /// `cgroup_name` is required when cgroup limits are configured.
    /// It must contain one normal path component.
    ///
    /// This direct low-level API has no retrying cleanup finalizer. If
    /// preparation leaves a safely pinned cgroup, error conversion performs
    /// one best-effort drop cleanup. The subprocess runner uses an internal
    /// ownership-preserving path and its bounded finalizer instead.
    ///
    /// # Errors
    ///
    /// Returns [`HostProcessError::InvalidConfig`] for a missing or unsafe cgroup name.
    /// Returns [`HostProcessError::Io`] when the cgroup cannot be created or configured.
    pub fn prepare_attempt(
        &self,
        cgroup_name: Option<&str>,
    ) -> Result<PreparedHostProcessAttempt, HostProcessError> {
        self.prepare_attempt_owned(cgroup_name)
            .map_err(AttemptPrepareFailure::into_error)
    }

    /// Prepares attempt resources without erasing residual cleanup ownership.
    pub(crate) fn prepare_attempt_owned(
        &self,
        cgroup_name: Option<&str>,
    ) -> Result<PreparedHostProcessAttempt, AttemptPrepareFailure> {
        let cgroup = match (&self.cgroups, cgroup_name) {
            (Some(cgroups), Some(name)) => {
                validate_cgroup_name(name).map_err(AttemptPrepareFailure::Clean)?;
                trace!(
                    event = "host_process.cgroup_prepare",
                    ?cgroups,
                    group = name,
                    "preparing host process cgroup"
                );
                let parent = self
                    .cgroup_parent
                    .as_ref()
                    .expect("prepared cgroup policy must have a parent");
                match prepare_cgroup_owned(parent, name, cgroups) {
                    Ok(cgroup) => Some(cgroup),
                    Err(CgroupPrepareFailure::Clean(error)) => {
                        return Err(AttemptPrepareFailure::Clean(error));
                    }
                    Err(CgroupPrepareFailure::Residual { source, cleanup }) => {
                        return Err(AttemptPrepareFailure::Residual {
                            error: source,
                            cleanup: cleanup.map(|cgroup| AttemptProcessDomain {
                                cgroup: Some(cgroup),
                            }),
                        });
                    }
                }
            }
            (Some(_), None) => {
                return Err(AttemptPrepareFailure::Clean(
                    HostProcessError::InvalidConfig(
                        "cgroup name is required when cgroup limits are configured".into(),
                    ),
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
    /// Converts resources prepared for a process that was never spawned into
    /// the same cleanup domain used after process attachment.
    #[cfg(feature = "subprocess")]
    pub(crate) fn into_cleanup_domain(self) -> AttemptProcessDomain {
        let Self {
            controls: _,
            cgroup,
        } = self;
        AttemptProcessDomain {
            cgroup: cgroup.map(PreparedCgroup::into_domain),
        }
    }

    /// Returns signal dispositions representable by the native macOS spawn path.
    ///
    /// `None` means that the attempt contains a control which requires the
    /// portable `fork`/`exec` fallback.
    #[cfg(all(feature = "subprocess", target_os = "macos"))]
    pub(crate) fn macos_spawn_signals(&self) -> Option<Arc<[libc::c_int]>> {
        let process = self.controls.process.as_ref();
        if process.is_some_and(PreparedProcessConfig::has_umask)
            || self
                .controls
                .rlimits
                .as_ref()
                .is_some_and(|limits| !limits.is_empty())
            || self.cgroup.is_some()
            || self
                .controls
                .security
                .as_ref()
                .is_some_and(|security| !security.is_empty())
        {
            return None;
        }

        Some(
            process
                .map(PreparedProcessConfig::reset_signals)
                .unwrap_or_else(|| Arc::from([])),
        )
    }

    /// Consumes an attempt handled entirely by native macOS spawn attributes.
    #[cfg(all(feature = "subprocess", target_os = "macos"))]
    pub(crate) fn into_macos_spawn_domain(self) -> AttemptProcessDomain {
        debug_assert!(self.macos_spawn_signals().is_some());
        let Self {
            controls: _,
            cgroup,
        } = self;
        debug_assert!(cgroup.is_none());
        AttemptProcessDomain { cgroup: None }
    }

    /// Attaches enabled controls to a process command.
    ///
    /// This token is consumed.
    /// The returned domain owns resources that require post-process cleanup.
    pub fn apply_to_command(self, command: &mut Command) -> AttemptProcessDomain {
        let Self { controls, cgroup } = self;
        let mut domain = AttemptProcessDomain { cgroup: None };

        if let Some(process) = &controls.process {
            trace!(
                event = "host_process.control_attach",
                control = "process",
                ?process,
                "attaching host process control"
            );
            attach_process_config(command, process);
        }
        if let Some(rlimits) = &controls.rlimits {
            trace!(
                event = "host_process.control_attach",
                control = "rlimits",
                ?rlimits,
                "attaching host process control"
            );
            attach_rlimits(command, rlimits);
        }
        if let Some(prepared) = cgroup {
            trace!(
                event = "host_process.control_attach",
                control = "cgroup",
                cgroup = %prepared.path().display(),
                "attaching host process control",
            );
            domain.cgroup = Some(attach_cgroup(command, prepared));
        }
        if let Some(security) = &controls.security {
            trace!(
                event = "host_process.control_attach",
                control = "security",
                credentials_configured = security.credentials.is_some(),
                drop_all_caps = security.drop_all_caps,
                kept_capability_count = security.keep_caps.len(),
                no_new_privs = security.no_new_privs,
                namespaces_configured = !security.namespaces.is_empty(),
                seccomp_configured = security.seccomp != crate::isolation::SeccompPolicy::Disabled,
                "attaching host process control"
            );
            attach_security(command, security);
        }

        domain
    }
}

fn validate_cgroup_name(name: &str) -> Result<(), HostProcessError> {
    let mut components = Path::new(name).components();
    let valid = !name.as_bytes().contains(&0)
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if valid {
        Ok(())
    } else {
        Err(HostProcessError::InvalidConfig(format!(
            "cgroup name must be one normal path component: {name:?}"
        )))
    }
}

#[cfg(feature = "host-process")]
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
    fn cgroup_name_is_one_safe_component() {
        assert!(validate_cgroup_name("attempt-1").is_ok());
        for name in [
            "",
            ".",
            "..",
            "nested/attempt",
            "/attempt",
            "attempt\0other",
        ] {
            assert!(validate_cgroup_name(name).is_err(), "accepted {name:?}");
        }
    }

    #[test]
    fn policy_components_are_available_to_backends() {
        let policy = HostProcessPolicy::new()
            .with_process_config(ProcessConfig {
                reset_signals: true,
                ..Default::default()
            })
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

        assert!(
            policy
                .process_config()
                .is_some_and(|process| process.reset_signals)
        );
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
        let requested = crate::host::reduced_nofile_limit_for_test();
        let policy = HostProcessPolicy::new()
            .with_rlimits(RlimitConfig {
                max_open_files: Some(requested),
                ..Default::default()
            })
            .prepare()
            .unwrap();
        assert_eq!(
            policy.rlimits().and_then(PreparedRlimits::max_open_files),
            Some(requested)
        );
        let attempt = policy.prepare_attempt(None).unwrap();
        let mut command = Command::new("sh");
        command.arg("-c").arg("ulimit -n");

        let _domain = attempt.apply_to_command(&mut command);
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
    fn attempt_process_domain_owns_cgroup_cleanup() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let mut domain = AttemptProcessDomain::for_test(path.clone());

        assert_eq!(domain.cgroup_path(), Some(path.as_path()));
        assert_eq!(
            domain.terminate_tree().unwrap(),
            DomainTermination::Unavailable
        );
        domain.cleanup().unwrap();
        domain.cleanup().unwrap();

        assert!(domain.cgroup_path().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn process_domain_without_cgroups_has_no_tree_termination() {
        let mut domain = AttemptProcessDomain { cgroup: None };
        assert_eq!(domain.cgroup_populated().unwrap(), None);
        assert_eq!(
            domain.terminate_tree().unwrap(),
            DomainTermination::Unavailable
        );
        domain.cleanup().unwrap();
    }

    #[cfg(all(feature = "subprocess", target_os = "macos"))]
    #[test]
    fn macos_spawn_accepts_native_controls_and_rejects_hook_only_controls() {
        let native = HostProcessPolicy::new()
            .with_process_config(ProcessConfig {
                reset_signals: true,
                new_session: true,
                umask: None,
            })
            .prepare()
            .unwrap()
            .prepare_attempt(None)
            .unwrap();
        assert!(!native.macos_spawn_signals().unwrap().is_empty());

        for fallback in [
            HostProcessPolicy::new().with_process_config(ProcessConfig {
                umask: Some(0o027),
                ..Default::default()
            }),
            HostProcessPolicy::new().with_rlimits(RlimitConfig {
                disable_core_dumps: true,
                ..Default::default()
            }),
        ] {
            let attempt = fallback.prepare().unwrap().prepare_attempt(None).unwrap();
            assert!(attempt.macos_spawn_signals().is_none());
        }
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
