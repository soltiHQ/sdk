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
//! set supplementary groups
//!   ▼
//! set real, effective, and saved gid
//!   ▼
//! set real, effective, and saved uid
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

#[cfg(feature = "host-process")]
use std::process::Command;

#[cfg(feature = "host-process")]
use crate::host::HostProcessError;
use crate::host::LinuxCapability;

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

    #[cfg(all(feature = "host-process", target_os = "linux"))]
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
    /// The default action remains `Allow`.
    /// This policy is not a syscall sandbox.
    /// It always enables `no_new_privs` before installing the filter.
    /// Host process enforcement requires feature `seccomp`.
    /// It supports Linux on LP64 `x86_64` and little-endian `aarch64`.
    DenyHostControl,
}

/// Exact Linux credentials applied before `execve`.
///
/// The user and group IDs are applied to the real, effective, and saved slots.
/// Supplementary groups are replaced by the provided list.
/// Empty supplementary groups clear the inherited list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCredentials {
    /// Real, effective, and saved user ID.
    pub uid: u32,
    /// Real, effective, and saved group ID.
    pub gid: u32,
    /// Exact supplementary group list.
    pub supplementary_groups: Vec<u32>,
}

impl ProcessCredentials {
    /// Creates credentials with no supplementary groups.
    pub fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid,
            gid,
            supplementary_groups: Vec::new(),
        }
    }

    /// Replaces the supplementary group list.
    pub fn with_supplementary_groups(mut self, supplementary_groups: impl Into<Vec<u32>>) -> Self {
        self.supplementary_groups = supplementary_groups.into();
        self
    }
}

/// Security controls applied to a host process.
///
/// Non-empty settings require Linux.
/// A failed control prevents process start.
///
/// | Field           | Effect                                                   |
/// |-----------------|----------------------------------------------------------|
/// | `namespaces`    | Creates selected Linux namespaces                        |
/// | `credentials`   | Replaces user, group, and supplementary group IDs         |
/// | `drop_all_caps` | Removes capabilities not listed in `keep_caps`           |
/// | `no_new_privs`  | Prevents privilege gain through `execve`                 |
/// | `seccomp`       | Installs the selected syscall policy                     |
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
    ///
    /// `false` does not clear an inherited setting.
    /// [`SeccompPolicy::DenyHostControl`] enables it regardless of this field.
    pub no_new_privs: bool,
    /// Exact credentials applied to the process.
    ///
    /// `None` preserves inherited credentials.
    pub credentials: Option<ProcessCredentials>,
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
            && self.credentials.is_none()
            && self.namespaces.is_empty()
            && self.seccomp == SeccompPolicy::Disabled
    }

    /// Returns whether this policy explicitly establishes `no_new_privs`.
    ///
    /// Seccomp filter installation requires this setting.
    /// A `false` result does not mean that an inherited setting is cleared.
    pub fn effective_no_new_privs(&self) -> bool {
        self.no_new_privs || self.seccomp != SeccompPolicy::Disabled
    }

    #[cfg(feature = "host-process")]
    pub(crate) fn validate(&self) -> Result<(), HostProcessError> {
        if !self.keep_caps.is_empty() && !self.drop_all_caps {
            return Err(HostProcessError::InvalidConfig(
                "security.keep_caps requires security.drop_all_caps".into(),
            ));
        }

        if let Some(credentials) = &self.credentials {
            validate_credentials(credentials)?;
        }

        #[cfg(not(feature = "seccomp"))]
        if self.seccomp != SeccompPolicy::Disabled {
            return Err(HostProcessError::InvalidConfig(
                "security.seccomp requires the `seccomp` feature".into(),
            ));
        }

        #[cfg(all(target_os = "linux", feature = "seccomp"))]
        if linux_impl::seccomp::build_program(&self.seccomp).is_err() {
            return Err(HostProcessError::InvalidConfig(
                "security.seccomp cannot be compiled for this target".into(),
            ));
        }

        Ok(())
    }
}

#[cfg(feature = "host-process")]
fn validate_credentials(credentials: &ProcessCredentials) -> Result<(), HostProcessError> {
    if credentials.uid == u32::MAX {
        return Err(HostProcessError::InvalidConfig(
            "security.credentials.uid cannot be the unchanged-ID sentinel".into(),
        ));
    }
    if credentials.gid == u32::MAX {
        return Err(HostProcessError::InvalidConfig(
            "security.credentials.gid cannot be the unchanged-ID sentinel".into(),
        ));
    }
    if credentials.supplementary_groups.contains(&u32::MAX) {
        return Err(HostProcessError::InvalidConfig(
            "security.credentials.supplementary_groups cannot contain the unchanged-ID sentinel"
                .into(),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        if credentials.supplementary_groups.len() > libc::c_int::MAX as usize {
            return Err(HostProcessError::InvalidConfig(
                "security.credentials.supplementary_groups is too large".into(),
            ));
        }
        // SAFETY: `sysconf` reads one process-wide numeric limit.
        let max = unsafe { libc::sysconf(libc::_SC_NGROUPS_MAX) };
        if max >= 0 && credentials.supplementary_groups.len() > max as usize {
            return Err(HostProcessError::InvalidConfig(format!(
                "security.credentials.supplementary_groups exceeds NGROUPS_MAX ({max})"
            )));
        }
    }

    Ok(())
}

#[cfg(feature = "host-process")]
pub(crate) fn attach_security(cmd: &mut Command, config: &SecurityConfig) {
    if config.is_empty() {
        return;
    }

    #[cfg(target_os = "linux")]
    linux_impl::attach(cmd, config);

    #[cfg(not(target_os = "linux"))]
    let _ = (cmd, config);
}

#[cfg(all(feature = "host-process", target_os = "linux"))]
mod linux_impl {
    use std::io;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    use super::{KeepMask, ProcessCredentials, SecurityConfig};
    use crate::host::log::{pre_exec_log, pre_exec_log_errno};

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    const PR_CAPBSET_READ: libc::c_int = 23;
    const PR_CAPBSET_DROP: libc::c_int = 24;
    const PR_SET_KEEPCAPS: libc::c_int = 8;
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    const PR_GET_NO_NEW_PRIVS: libc::c_int = 39;
    const CAP_LAST_CAP: u32 = 63;

    #[cfg(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc"))]
    const SYS_SETGROUPS: libc::c_long = libc::SYS_setgroups32;
    #[cfg(not(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc")))]
    const SYS_SETGROUPS: libc::c_long = libc::SYS_setgroups;
    #[cfg(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc"))]
    const SYS_GETGROUPS: libc::c_long = libc::SYS_getgroups32;
    #[cfg(not(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc")))]
    const SYS_GETGROUPS: libc::c_long = libc::SYS_getgroups;
    #[cfg(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc"))]
    const SYS_SETRESGID: libc::c_long = libc::SYS_setresgid32;
    #[cfg(not(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc")))]
    const SYS_SETRESGID: libc::c_long = libc::SYS_setresgid;
    #[cfg(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc"))]
    const SYS_GETRESGID: libc::c_long = libc::SYS_getresgid32;
    #[cfg(not(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc")))]
    const SYS_GETRESGID: libc::c_long = libc::SYS_getresgid;
    #[cfg(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc"))]
    const SYS_SETRESUID: libc::c_long = libc::SYS_setresuid32;
    #[cfg(not(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc")))]
    const SYS_SETRESUID: libc::c_long = libc::SYS_setresuid;
    #[cfg(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc"))]
    const SYS_GETRESUID: libc::c_long = libc::SYS_getresuid32;
    #[cfg(not(any(target_arch = "x86", target_arch = "arm", target_arch = "sparc")))]
    const SYS_GETRESUID: libc::c_long = libc::SYS_getresuid;

    struct PreparedCredentials {
        uid: libc::uid_t,
        gid: libc::gid_t,
        supplementary_groups: Vec<libc::gid_t>,
        observed_groups: Vec<libc::gid_t>,
        group_count: libc::c_int,
    }

    impl From<&ProcessCredentials> for PreparedCredentials {
        fn from(credentials: &ProcessCredentials) -> Self {
            let mut supplementary_groups = credentials
                .supplementary_groups
                .iter()
                .map(|&gid| gid as libc::gid_t)
                .collect::<Vec<_>>();
            supplementary_groups.sort_unstable();
            Self {
                uid: credentials.uid as libc::uid_t,
                gid: credentials.gid as libc::gid_t,
                observed_groups: vec![0; supplementary_groups.len()],
                group_count: supplementary_groups.len() as libc::c_int,
                supplementary_groups,
            }
        }
    }

    pub(super) fn attach(cmd: &mut Command, config: &SecurityConfig) {
        let keep_mask = KeepMask::from_caps(&config.keep_caps);
        let drop_all_caps = config.drop_all_caps;
        let no_new_privs = config.effective_no_new_privs();
        let namespace_mask = config.namespaces.mask();
        let mut credentials = config.credentials.as_ref().map(PreparedCredentials::from);

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
                    if credentials.is_some() && !keep_mask.is_empty() {
                        keep_capabilities_across_uid_change()?;
                    }
                }

                if let Some(credentials) = &mut credentials {
                    apply_credentials(credentials)?;
                }

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

    fn apply_credentials(credentials: &mut PreparedCredentials) -> io::Result<()> {
        let groups = &credentials.supplementary_groups;
        let groups_ptr = if groups.is_empty() {
            std::ptr::null()
        } else {
            groups.as_ptr()
        };

        // SAFETY: the list is immutable storage prepared before `fork`.
        if unsafe { libc::syscall(SYS_SETGROUPS, groups.len(), groups_ptr) } != 0 {
            return logged_last_error(b"solti-exec: setgroups failed: ");
        }
        verify_supplementary_groups(credentials)?;

        let gid = credentials.gid;
        // SAFETY: all three ids are validated concrete group IDs.
        if unsafe { libc::syscall(SYS_SETRESGID, gid, gid, gid) } != 0 {
            return logged_last_error(b"solti-exec: setresgid failed: ");
        }
        verify_group_ids(gid)?;

        let uid = credentials.uid;
        // SAFETY: all three ids are validated concrete user IDs.
        if unsafe { libc::syscall(SYS_SETRESUID, uid, uid, uid) } != 0 {
            return logged_last_error(b"solti-exec: setresuid failed: ");
        }
        verify_user_ids(uid)
    }

    fn verify_supplementary_groups(credentials: &mut PreparedCredentials) -> io::Result<()> {
        let observed_ptr = if credentials.observed_groups.is_empty() {
            std::ptr::null_mut()
        } else {
            credentials.observed_groups.as_mut_ptr()
        };
        // SAFETY: the output buffer was allocated to `group_count` elements before `fork`.
        let count = unsafe { libc::syscall(SYS_GETGROUPS, credentials.group_count, observed_ptr) };
        if count < 0 {
            return logged_last_error(b"solti-exec: getgroups failed: ");
        }
        if count != libc::c_long::from(credentials.group_count) {
            pre_exec_log(b"solti-exec: supplementary group count verification failed\n");
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }

        // The buffer was allocated in the parent. `sort_unstable` is in-place.
        credentials.observed_groups.sort_unstable();
        if credentials.observed_groups != credentials.supplementary_groups {
            pre_exec_log(b"solti-exec: supplementary group verification failed\n");
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(())
    }

    fn verify_group_ids(expected: libc::gid_t) -> io::Result<()> {
        let mut real = 0;
        let mut effective = 0;
        let mut saved = 0;
        // SAFETY: all output pointers refer to stack-local group IDs.
        if unsafe { libc::syscall(SYS_GETRESGID, &mut real, &mut effective, &mut saved) } != 0 {
            return logged_last_error(b"solti-exec: getresgid failed: ");
        }
        if [real, effective, saved] != [expected; 3] {
            pre_exec_log(b"solti-exec: group credential verification failed\n");
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(())
    }

    fn verify_user_ids(expected: libc::uid_t) -> io::Result<()> {
        let mut real = 0;
        let mut effective = 0;
        let mut saved = 0;
        // SAFETY: all output pointers refer to stack-local user IDs.
        if unsafe { libc::syscall(SYS_GETRESUID, &mut real, &mut effective, &mut saved) } != 0 {
            return logged_last_error(b"solti-exec: getresuid failed: ");
        }
        if [real, effective, saved] != [expected; 3] {
            pre_exec_log(b"solti-exec: user credential verification failed\n");
            return Err(io::Error::from_raw_os_error(libc::EPERM));
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
        // SAFETY: `prctl` receives scalar arguments only.
        let state = unsafe { libc::prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) };
        if state < 0 {
            return logged_last_error(b"solti-exec: PR_GET_NO_NEW_PRIVS failed: ");
        }
        if state != 1 {
            pre_exec_log(b"solti-exec: no_new_privs verification failed\n");
            return Err(io::Error::from_raw_os_error(libc::EPERM));
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
        use crate::host::log::pre_exec_log;

        fn dangerous_syscalls() -> &'static [libc::c_long] {
            &[
                libc::SYS_ptrace,
                libc::SYS_process_vm_readv,
                libc::SYS_process_vm_writev,
                libc::SYS_process_madvise,
                libc::SYS_pidfd_getfd,
                libc::SYS_kcmp,
                libc::SYS_mount,
                libc::SYS_umount2,
                libc::SYS_pivot_root,
                libc::SYS_open_tree,
                libc::SYS_move_mount,
                libc::SYS_fsopen,
                libc::SYS_fsconfig,
                libc::SYS_fsmount,
                libc::SYS_fspick,
                libc::SYS_mount_setattr,
                libc::SYS_name_to_handle_at,
                libc::SYS_open_by_handle_at,
                libc::SYS_quotactl,
                libc::SYS_quotactl_fd,
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
                libc::SYS_sethostname,
                libc::SYS_setdomainname,
                libc::SYS_settimeofday,
                libc::SYS_clock_settime,
                libc::SYS_adjtimex,
                libc::SYS_clock_adjtime,
                libc::SYS_setns,
                libc::SYS_acct,
                libc::SYS_syslog,
                libc::SYS_fanotify_init,
                libc::SYS_lookup_dcookie,
                libc::SYS_vhangup,
                libc::SYS_add_key,
                libc::SYS_keyctl,
                libc::SYS_request_key,
                #[cfg(target_arch = "x86_64")]
                libc::SYS_iopl,
                #[cfg(target_arch = "x86_64")]
                libc::SYS_ioperm,
            ]
        }

        #[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
        const ARCH: Option<TargetArch> = Some(TargetArch::x86_64);
        #[cfg(all(
            target_arch = "aarch64",
            target_endian = "little",
            target_pointer_width = "64"
        ))]
        const ARCH: Option<TargetArch> = Some(TargetArch::aarch64);
        #[cfg(not(any(
            all(target_arch = "x86_64", target_pointer_width = "64"),
            all(
                target_arch = "aarch64",
                target_endian = "little",
                target_pointer_width = "64"
            )
        )))]
        const ARCH: Option<TargetArch> = None;

        pub(crate) fn build_program(policy: &SeccompPolicy) -> Result<Option<BpfProgram>, ()> {
            match policy {
                SeccompPolicy::Disabled => Ok(None),
                SeccompPolicy::DenyHostControl => {
                    let architecture = ARCH.ok_or(())?;
                    build_denylist(architecture).map(Some).map_err(|_| ())
                }
            }
        }

        fn build_denylist(
            architecture: TargetArch,
        ) -> Result<BpfProgram, Box<dyn std::error::Error>> {
            let mut rules = BTreeMap::new();
            for &syscall in dangerous_syscalls() {
                #[allow(clippy::useless_conversion)]
                rules.insert(i64::from(syscall), Vec::new());
            }
            #[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
            // Linux before 5.4 accepted these confused x32 syscall numbers
            // without X32_SYSCALL_BIT. They are reserved on newer kernels.
            for syscall in 512..=547 {
                rules.insert(syscall, Vec::new());
            }
            let filter = SeccompFilter::new(
                rules,
                SeccompAction::Allow,
                SeccompAction::Errno(libc::EPERM as u32),
                architecture,
            )?;
            let mut program = filter.try_into()?;
            #[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
            deny_x32_abi(&mut program)?;
            Ok(program)
        }

        #[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
        fn deny_x32_abi(program: &mut BpfProgram) -> io::Result<()> {
            const SYSCALL_LOAD_INDEX: usize = 3;
            const FIRST_RULE_INDEX: usize = 4;
            const BPF_LOAD_WORD_ABSOLUTE: u16 = 0x20;
            const BPF_JUMP_GREATER_OR_EQUAL: u16 = 0x35;
            const BPF_RETURN_CONSTANT: u16 = 0x06;
            const X32_SYSCALL_BIT: u32 = 0x4000_0000;

            let syscall_load = program
                .get(SYSCALL_LOAD_INDEX)
                .ok_or_else(|| io::Error::other("seccomp program has no syscall-number load"))?;
            if syscall_load.code != BPF_LOAD_WORD_ABSOLUTE
                || syscall_load.jt != 0
                || syscall_load.jf != 0
                || syscall_load.k != 0
            {
                return Err(io::Error::other(
                    "seccomp program has an unexpected syscall-number load",
                ));
            }

            program.insert(
                FIRST_RULE_INDEX,
                seccompiler::sock_filter {
                    code: BPF_JUMP_GREATER_OR_EQUAL,
                    jt: 0,
                    jf: 1,
                    k: X32_SYSCALL_BIT,
                },
            );
            program.insert(
                FIRST_RULE_INDEX + 1,
                seccompiler::sock_filter {
                    code: BPF_RETURN_CONSTANT,
                    jt: 0,
                    jf: 0,
                    k: u32::from(SeccompAction::Errno(libc::EPERM as u32)),
                },
            );
            Ok(())
        }

        pub(super) fn install(program: &BpfProgram) -> io::Result<()> {
            seccompiler::apply_filter(program).map_err(|_| {
                pre_exec_log(b"solti-exec: seccomp installation failed\n");
                io::Error::from_raw_os_error(libc::EPERM)
            })
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn denylist_contains_expected_host_control_groups() {
                for syscall in [
                    libc::SYS_mount,
                    libc::SYS_umount2,
                    libc::SYS_pivot_root,
                    libc::SYS_open_tree,
                    libc::SYS_move_mount,
                    libc::SYS_fsopen,
                    libc::SYS_fsconfig,
                    libc::SYS_fsmount,
                    libc::SYS_fspick,
                    libc::SYS_mount_setattr,
                    libc::SYS_sethostname,
                    libc::SYS_clock_settime,
                    libc::SYS_open_by_handle_at,
                    libc::SYS_process_vm_writev,
                    libc::SYS_pidfd_getfd,
                ] {
                    assert!(dangerous_syscalls().contains(&syscall));
                }
            }

            #[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
            #[test]
            fn denylist_rejects_the_x32_abi_before_syscall_rules() {
                let program = build_program(&SeccompPolicy::DenyHostControl)
                    .unwrap()
                    .unwrap();
                let guard = &program[4..6];

                assert_eq!(guard[0].code, 0x35);
                assert_eq!((guard[0].jt, guard[0].jf), (0, 1));
                assert_eq!(guard[0].k, 0x4000_0000);
                assert_eq!(guard[1].code, 0x06);
                assert_eq!(
                    guard[1].k,
                    u32::from(SeccompAction::Errno(libc::EPERM as u32))
                );
            }
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

#[cfg(all(feature = "host-process", target_os = "linux"))]
#[derive(Clone, Copy)]
struct KeepMask {
    bits: [u32; 2],
}

#[cfg(all(feature = "host-process", target_os = "linux"))]
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
    fn process_credentials_require_both_primary_ids() {
        let credentials =
            ProcessCredentials::new(1000, 1001).with_supplementary_groups(vec![10, 20]);
        assert_eq!(credentials.uid, 1000);
        assert_eq!(credentials.gid, 1001);
        assert_eq!(credentials.supplementary_groups, [10, 20]);
    }

    #[test]
    fn seccomp_explicitly_enables_no_new_privs() {
        let config = SecurityConfig {
            seccomp: SeccompPolicy::DenyHostControl,
            ..Default::default()
        };
        assert!(config.effective_no_new_privs());
        assert!(!SecurityConfig::default().effective_no_new_privs());
    }

    #[test]
    #[cfg(feature = "host-process")]
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
    #[cfg(feature = "host-process")]
    fn unchanged_id_sentinels_are_rejected() {
        for credentials in [
            ProcessCredentials::new(u32::MAX, 1000),
            ProcessCredentials::new(1000, u32::MAX),
            ProcessCredentials::new(1000, 1000).with_supplementary_groups(vec![u32::MAX]),
        ] {
            let config = SecurityConfig {
                credentials: Some(credentials),
                ..Default::default()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("unchanged-ID sentinel"), "got: {error}");
        }
    }

    #[test]
    #[cfg(all(feature = "host-process", target_os = "linux"))]
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

    #[cfg(all(feature = "host-process", target_os = "linux"))]
    #[test]
    fn no_new_privs_works_without_root() {
        let config = SecurityConfig {
            no_new_privs: true,
            ..Default::default()
        };
        let mut command = Command::new("true");
        attach_security(&mut command, &config);
        assert!(command.status().unwrap().success());
    }

    #[cfg(all(feature = "host-process", not(feature = "seccomp")))]
    #[test]
    fn seccomp_requires_feature() {
        let config = SecurityConfig {
            seccomp: SeccompPolicy::DenyHostControl,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[cfg(all(feature = "host-process", target_os = "linux", feature = "seccomp"))]
    #[test]
    fn denylist_compiles() {
        assert!(matches!(
            linux_impl::seccomp::build_program(&SeccompPolicy::DenyHostControl),
            Ok(Some(_))
        ));
    }
}
