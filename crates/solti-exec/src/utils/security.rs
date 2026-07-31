//! # Linux process security
//!
//! [`SecurityConfig`] applies process controls before `execve`.
//!
//! ## Enabled Control Order
//!
//! ```text
//! fork
//!   ▼
//! create namespaces
//!   ▼
//! reduce capability bounding set
//!   ▼
//! set gid and uid
//!   ▼
//! set effective and ambient capabilities
//!   ▼
//! no_new_privs
//!   ▼
//! seccomp
//!   ▼
//! execve
//! ```
//!
//! Configured controls are fail-closed.
//! A failed control prevents the child from starting.
//! A non-empty policy is rejected on non-Linux platforms.

use tokio::process::Command;

use crate::utils::LinuxCapability;

/// Linux namespaces created for the child before `execve`.
///
/// PID and user namespaces are not configured by this type.
#[derive(Debug, Clone, Copy, Default)]
pub struct Namespaces {
    /// Creates a mount namespace and makes mount propagation private.
    pub mount: bool,
    /// Creates a network namespace.
    pub net: bool,
    /// Creates an IPC namespace.
    pub ipc: bool,
    /// Creates a UTS namespace.
    pub uts: bool,
    /// Creates a cgroup namespace.
    pub cgroup: bool,
}

impl Namespaces {
    /// Returns `true` when no namespace is requested.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !(self.mount || self.net || self.ipc || self.uts || self.cgroup)
    }

    #[cfg(target_os = "linux")]
    fn mask(&self) -> libc::c_int {
        let mut mask = 0;
        if self.mount {
            mask |= libc::CLONE_NEWNS;
        }
        if self.net {
            mask |= libc::CLONE_NEWNET;
        }
        if self.ipc {
            mask |= libc::CLONE_NEWIPC;
        }
        if self.uts {
            mask |= libc::CLONE_NEWUTS;
        }
        if self.cgroup {
            mask |= libc::CLONE_NEWCGROUP;
        }
        mask
    }
}

/// Seccomp policy applied before `execve`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SeccompPolicy {
    /// Does not install a syscall filter.
    #[default]
    Disabled,
    /// Rejects a fixed host-control syscall denylist with `EPERM`.
    ///
    /// Other syscalls remain allowed.
    /// This variant requires feature `seccomp`.
    /// It supports Linux on `x86_64` and `aarch64`.
    BlockDangerous,
}

/// Security policy for a subprocess.
///
/// Non-empty settings require Linux.
/// A failed control prevents process start.
///
/// | Field           | Effect                                                |
/// |-----------------|-------------------------------------------------------|
/// | `namespaces`    | Creates selected Linux namespaces                     |
/// | `run_as_gid`    | Clears supplementary groups and changes group id      |
/// | `run_as_uid`    | Clears supplementary groups and changes user id       |
/// | `drop_all_caps` | Removes capabilities not listed in `keep_caps`        |
/// | `no_new_privs`  | Prevents privilege gain through `execve`              |
/// | `seccomp`       | Installs the selected syscall policy                  |
///
/// `keep_caps` requires `drop_all_caps`.
/// Requested capabilities must already be available to the agent process.
#[derive(Debug, Clone, Default)]
pub struct SecurityConfig {
    /// Drops every Linux capability not listed in [`Self::keep_caps`].
    pub drop_all_caps: bool,
    /// Capabilities retained by [`Self::drop_all_caps`].
    pub keep_caps: Vec<LinuxCapability>,
    /// Prevents privilege gain through `execve`.
    pub no_new_privs: bool,
    /// Clears supplementary groups and changes to this group id.
    pub run_as_gid: Option<u32>,
    /// Clears supplementary groups and changes to this user id.
    pub run_as_uid: Option<u32>,
    /// Namespaces created before identity and capability changes.
    pub namespaces: Namespaces,
    /// Syscall policy installed immediately before `execve`.
    pub seccomp: SeccompPolicy,
}

impl SecurityConfig {
    /// Returns `true` when the policy does not request any control.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.drop_all_caps
            && self.keep_caps.is_empty()
            && !self.no_new_privs
            && self.run_as_uid.is_none()
            && self.run_as_gid.is_none()
            && self.namespaces.is_empty()
            && self.seccomp == SeccompPolicy::Disabled
    }

    pub(crate) fn validate(&self) -> Result<(), crate::ExecError> {
        if !self.keep_caps.is_empty() && !self.drop_all_caps {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "security.keep_caps requires security.drop_all_caps".into(),
            ));
        }

        #[cfg(not(feature = "seccomp"))]
        if self.seccomp != SeccompPolicy::Disabled {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "security.seccomp requires the `seccomp` feature".into(),
            ));
        }

        #[cfg(all(target_os = "linux", feature = "seccomp"))]
        if linux_impl::seccomp::build_program(&self.seccomp).is_err() {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "security.seccomp cannot be compiled for this target".into(),
            ));
        }

        Ok(())
    }
}

pub(crate) fn attach_security(cmd: &mut Command, config: &SecurityConfig) {
    if config.is_empty() {
        return;
    }

    #[cfg(target_os = "linux")]
    linux_impl::attach(cmd, config);

    #[cfg(not(target_os = "linux"))]
    let _ = (cmd, config);
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::io;

    use tokio::process::Command;

    use super::{KeepMask, SecurityConfig};
    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    const PR_CAPBSET_READ: libc::c_int = 23;
    const PR_CAPBSET_DROP: libc::c_int = 24;
    const PR_SET_KEEPCAPS: libc::c_int = 8;
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    const CAP_LAST_CAP: u32 = 63;

    pub(super) fn attach(cmd: &mut Command, config: &SecurityConfig) {
        let keep_mask = KeepMask::from_caps(&config.keep_caps);
        let drop_all_caps = config.drop_all_caps;
        let no_new_privs = config.no_new_privs;
        let namespace_mask = config.namespaces.mask();
        let run_as_gid = config.run_as_gid;
        let run_as_uid = config.run_as_uid;

        #[cfg(feature = "seccomp")]
        let seccomp_program = seccomp::build_program(&config.seccomp);

        // SAFETY: the hook calls raw process-control syscalls only. Captured
        // configuration is prepared in the parent before `fork`.
        unsafe {
            cmd.pre_exec(move || {
                if namespace_mask != 0 {
                    apply_namespaces(namespace_mask)?;
                }

                if drop_all_caps {
                    drop_bounding_capabilities(keep_mask)?;
                    if run_as_uid.is_some() && !keep_mask.is_empty() {
                        keep_capabilities_across_uid_change()?;
                    }
                }

                drop_privileges(run_as_gid, run_as_uid)?;

                if drop_all_caps {
                    drop_capabilities_batch(keep_mask)?;
                }

                if no_new_privs {
                    apply_no_new_privs()?;
                }

                #[cfg(feature = "seccomp")]
                match &seccomp_program {
                    Ok(Some(program)) => seccomp::install(program)?,
                    Ok(None) => {}
                    Err(()) => {
                        pre_exec_log(b"solti-exec: seccomp program unavailable\n");
                        return Err(io::Error::from_raw_os_error(libc::EPERM));
                    }
                }

                Ok(())
            });
        }
    }

    fn apply_namespaces(mask: libc::c_int) -> io::Result<()> {
        // SAFETY: `unshare` accepts a scalar mask.
        if unsafe { libc::unshare(mask) } != 0 {
            return logged_last_error(b"solti-exec: unshare failed: ");
        }

        if mask & libc::CLONE_NEWNS != 0 {
            let root = b"/\0";
            // SAFETY: all pointers are either null or point to a static C string.
            let result = unsafe {
                libc::mount(
                    std::ptr::null(),
                    root.as_ptr().cast(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )
            };
            if result != 0 {
                return logged_last_error(b"solti-exec: private mount propagation failed: ");
            }
        }

        Ok(())
    }

    fn drop_bounding_capabilities(keep_mask: KeepMask) -> io::Result<()> {
        for capability in 0..=CAP_LAST_CAP {
            // SAFETY: `prctl` receives scalar arguments only.
            let present = unsafe { libc::prctl(PR_CAPBSET_READ, capability, 0, 0, 0) };
            if present < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                log_error(b"solti-exec: capability bounding-set read failed: ", &error);
                return Err(error);
            }

            if present == 1 && !keep_mask.is_set(capability) {
                // SAFETY: `prctl` receives scalar arguments only.
                if unsafe { libc::prctl(PR_CAPBSET_DROP, capability, 0, 0, 0) } != 0 {
                    return logged_last_error(b"solti-exec: capability bounding-set drop failed: ");
                }
            }
        }
        Ok(())
    }

    fn keep_capabilities_across_uid_change() -> io::Result<()> {
        // SAFETY: `prctl` receives scalar arguments only.
        if unsafe { libc::prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0) } != 0 {
            return logged_last_error(b"solti-exec: PR_SET_KEEPCAPS failed: ");
        }
        Ok(())
    }

    fn drop_privileges(gid: Option<u32>, uid: Option<u32>) -> io::Result<()> {
        if gid.is_some() || uid.is_some() {
            // SAFETY: count zero with a null list clears supplementary groups.
            if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
                return logged_last_error(b"solti-exec: setgroups failed: ");
            }
        }

        if let Some(gid) = gid {
            // SAFETY: `setgid` receives a scalar id.
            if unsafe { libc::setgid(gid as libc::gid_t) } != 0 {
                return logged_last_error(b"solti-exec: setgid failed: ");
            }
        }

        if let Some(uid) = uid {
            // SAFETY: `setuid` receives a scalar id.
            if unsafe { libc::setuid(uid as libc::uid_t) } != 0 {
                return logged_last_error(b"solti-exec: setuid failed: ");
            }
        }

        Ok(())
    }

    fn drop_capabilities_batch(keep_mask: KeepMask) -> io::Result<()> {
        clear_ambient_capabilities()?;

        let mut header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapUserData::default(); 2];

        // SAFETY: the structs match Linux capability ABI version 3.
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            return logged_last_error(b"solti-exec: capget failed: ");
        }

        for (entry, allowed) in data.iter_mut().zip(keep_mask.bits) {
            if entry.permitted & allowed != allowed {
                pre_exec_log(b"solti-exec: requested capability is not permitted\n");
                return Err(io::Error::from_raw_os_error(libc::EPERM));
            }
            entry.effective = allowed;
            entry.permitted = allowed;
            entry.inheritable = allowed;
        }

        // SAFETY: the structs match Linux capability ABI version 3.
        if unsafe { capset(&mut header, data.as_ptr()) } != 0 {
            return logged_last_error(b"solti-exec: capset failed: ");
        }

        for capability in 0..=CAP_LAST_CAP {
            if keep_mask.is_set(capability) {
                raise_ambient_capability(capability)?;
            }
        }

        Ok(())
    }

    fn clear_ambient_capabilities() -> io::Result<()> {
        // SAFETY: `prctl` receives scalar arguments only.
        if unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) {
                log_error(b"solti-exec: clear ambient capabilities failed: ", &error);
                return Err(error);
            }
        }
        Ok(())
    }

    fn raise_ambient_capability(capability: u32) -> io::Result<()> {
        // SAFETY: `prctl` receives scalar arguments only.
        if unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, capability, 0, 0) } != 0 {
            return logged_last_error(b"solti-exec: raise ambient capability failed: ");
        }
        Ok(())
    }

    fn apply_no_new_privs() -> io::Result<()> {
        // SAFETY: `prctl` receives scalar arguments only.
        if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return logged_last_error(b"solti-exec: PR_SET_NO_NEW_PRIVS failed: ");
        }
        Ok(())
    }

    fn logged_last_error(prefix: &[u8]) -> io::Result<()> {
        let error = io::Error::last_os_error();
        log_error(prefix, &error);
        Err(error)
    }

    fn log_error(prefix: &[u8], error: &io::Error) {
        pre_exec_log(prefix);
        if let Some(code) = error.raw_os_error() {
            pre_exec_log_errno(code);
        }
    }

    #[cfg(feature = "seccomp")]
    pub(super) mod seccomp {
        use std::collections::BTreeMap;
        use std::io;

        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};

        use super::super::SeccompPolicy;
        use crate::utils::log::pre_exec_log;

        fn dangerous_syscalls() -> &'static [libc::c_long] {
            &[
                libc::SYS_ptrace,
                libc::SYS_mount,
                libc::SYS_umount2,
                libc::SYS_pivot_root,
                libc::SYS_kexec_load,
                libc::SYS_kexec_file_load,
                libc::SYS_init_module,
                libc::SYS_finit_module,
                libc::SYS_delete_module,
                libc::SYS_bpf,
                libc::SYS_perf_event_open,
                libc::SYS_swapon,
                libc::SYS_swapoff,
                libc::SYS_reboot,
                libc::SYS_setns,
                libc::SYS_acct,
                libc::SYS_add_key,
                libc::SYS_keyctl,
                libc::SYS_request_key,
            ]
        }

        #[cfg(target_arch = "x86_64")]
        const ARCH: Option<TargetArch> = Some(TargetArch::x86_64);
        #[cfg(target_arch = "aarch64")]
        const ARCH: Option<TargetArch> = Some(TargetArch::aarch64);
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        const ARCH: Option<TargetArch> = None;

        pub(crate) fn build_program(policy: &SeccompPolicy) -> Result<Option<BpfProgram>, ()> {
            match policy {
                SeccompPolicy::Disabled => Ok(None),
                SeccompPolicy::BlockDangerous => {
                    let architecture = ARCH.ok_or(())?;
                    build_blocklist(architecture).map(Some).map_err(|_| ())
                }
            }
        }

        fn build_blocklist(
            architecture: TargetArch,
        ) -> Result<BpfProgram, Box<dyn std::error::Error>> {
            let mut rules = BTreeMap::new();
            for &syscall in dangerous_syscalls() {
                #[allow(clippy::useless_conversion)]
                rules.insert(i64::from(syscall), Vec::new());
            }
            let filter = SeccompFilter::new(
                rules,
                SeccompAction::Allow,
                SeccompAction::Errno(libc::EPERM as u32),
                architecture,
            )?;
            Ok(filter.try_into()?)
        }

        pub(super) fn install(program: &BpfProgram) -> io::Result<()> {
            seccompiler::apply_filter(program).map_err(|_| {
                pre_exec_log(b"solti-exec: seccomp installation failed\n");
                io::Error::from_raw_os_error(libc::EPERM)
            })
        }
    }

    #[repr(C)]
    struct CapUserHeader {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct CapUserData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    unsafe extern "C" {
        fn capset(header: *mut CapUserHeader, data: *const CapUserData) -> libc::c_int;
        fn capget(header: *mut CapUserHeader, data: *mut CapUserData) -> libc::c_int;
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct KeepMask {
    bits: [u32; 2],
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl KeepMask {
    fn from_caps(capabilities: &[LinuxCapability]) -> Self {
        let mut bits = [0; 2];
        for capability in capabilities {
            let value = capability.to_cap_value();
            let index = (value / 32) as usize;
            if index < bits.len() {
                bits[index] |= 1 << (value % 32);
            }
        }
        Self { bits }
    }

    fn is_empty(self) -> bool {
        self.bits == [0, 0]
    }

    fn is_set(self, capability: u32) -> bool {
        let index = (capability / 32) as usize;
        index < self.bits.len() && self.bits[index] & (1 << (capability % 32)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_empty() {
        assert!(SecurityConfig::default().is_empty());
    }

    #[test]
    fn keep_capabilities_require_drop_policy() {
        let invalid = SecurityConfig {
            keep_caps: vec![LinuxCapability::NetBindService],
            ..Default::default()
        };
        let error = invalid.validate().unwrap_err().to_string();
        assert!(error.contains("keep_caps"));
        assert!(error.contains("drop_all_caps"));

        let valid = SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetBindService],
            ..Default::default()
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn keep_mask_maps_capability_numbers() {
        let mask = KeepMask::from_caps(&[
            LinuxCapability::Chown,
            LinuxCapability::NetBindService,
            LinuxCapability::SysAdmin,
        ]);
        assert!(mask.is_set(0));
        assert!(mask.is_set(10));
        assert!(mask.is_set(21));
        assert!(!mask.is_set(1));
        assert!(!mask.is_set(64));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_new_privs_works_without_root() {
        let config = SecurityConfig {
            no_new_privs: true,
            ..Default::default()
        };
        let mut command = Command::new("true");
        attach_security(&mut command, &config);
        assert!(command.status().await.unwrap().success());
    }

    #[cfg(not(feature = "seccomp"))]
    #[test]
    fn seccomp_requires_feature() {
        let config = SecurityConfig {
            seccomp: SeccompPolicy::BlockDangerous,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[cfg(all(target_os = "linux", feature = "seccomp"))]
    #[test]
    fn blocklist_compiles() {
        assert!(matches!(
            linux_impl::seccomp::build_program(&SeccompPolicy::BlockDangerous),
            Ok(Some(_))
        ));
    }
}
