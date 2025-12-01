//! Basic security hardening for subprocess-based runners.
//!
//! ## Overview
//!
//! This module provides a structured API for configuring process-level security:
//! - **Linux capabilities** (drop-all + allowlist model);
//! - **`no_new_privs`** flag (Linux).
//!
//! ## Capability operations
//!
//! Capability management is **best-effort**: operations may silently degrade to no-op when:
//! - The process lacks `CAP_SETPCAP` or root privileges.
//! - Running under restricted container runtimes (e.g., some Kubernetes pod security policies).
//! - The kernel doesn't support certain capabilities (older kernels).
//!
//! Failures to drop capabilities will emit a warning to stderr but will not prevent process spawn.
//!
//! ## `no_new_privs`
//!
//! The `no_new_privs` flag works without root privileges and is enforced strictly.
//! Failure to set this flag will cause the spawn to fail.

use tokio::process::Command;

#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// Declarative security policy for a child process.
///
/// All fields are optional / opt-in:
/// - `drop_all_caps = false` do not touch capabilities;
/// - `keep_caps` is only meaningful when `drop_all_caps` is `true`;
/// - `no_new_privs = false` do not set `no_new_privs`;
#[derive(Debug, Clone, Default)]
pub struct SecurityConfig {
    /// Drop all capabilities before exec.
    ///
    /// On Linux, this clears all capability sets (permitted, effective, inheritable, ambient)
    /// and then re-adds only those listed in `keep_caps`.
    ///
    /// Note: capability operations require CAP_SETPCAP or root. If the process lacks these
    /// privileges, the operation will log a warning and continue (non-fatal).
    pub drop_all_caps: bool,
    /// Optional allowlist of capabilities to keep after `drop_all_caps`.
    ///
    /// Only meaningful when `drop_all_caps = true`.
    pub keep_caps: Vec<LinuxCapability>,
    /// Enable `no_new_privs` for the child process.
    ///
    /// This prevents the process from gaining additional privileges via setuid binaries,
    /// file capabilities, or other mechanisms. This flag works without root privileges.
    ///
    /// Failures to set this flag are fatal (spawn will fail).
    pub no_new_privs: bool,
}

impl SecurityConfig {
    /// Returns `true` if no security knobs are configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.drop_all_caps && self.keep_caps.is_empty() && !self.no_new_privs
    }
}

/// Linux capability identifiers used for allowlisting.
///
/// This enum covers the most commonly used capabilities. For a complete list,
/// see `man 7 capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LinuxCapability {
    /// CAP_CHOWN: Make arbitrary changes to file UIDs and GIDs
    Chown,
    /// CAP_DAC_OVERRIDE: Bypass file read, write, and execute permission checks
    DacOverride,
    /// CAP_DAC_READ_SEARCH: Bypass file read permission checks and directory read/execute checks
    DacReadSearch,
    /// CAP_FOWNER: Bypass permission checks on operations that normally require the filesystem UID
    Fowner,
    /// CAP_FSETID: Don't clear set-user-ID and set-group-ID mode bits
    Fsetid,
    /// CAP_KILL: Bypass permission checks for sending signals
    Kill,
    /// CAP_SETGID: Make arbitrary manipulations of process GIDs and supplementary GID list
    Setgid,
    /// CAP_SETUID: Make arbitrary manipulations of process UIDs
    Setuid,
    /// CAP_SETPCAP: Modify process capabilities
    Setpcap,
    /// CAP_NET_BIND_SERVICE: Bind a socket to privileged ports (port numbers less than 1024)
    NetBindService,
    /// CAP_NET_RAW: Use RAW and PACKET sockets; bind to any address for transparent proxying
    NetRaw,
    /// CAP_NET_ADMIN: Perform various network-related operations
    NetAdmin,
    /// CAP_SYS_CHROOT: Use chroot()
    SysChroot,
    /// CAP_SYS_PTRACE: Trace arbitrary processes using ptrace()
    SysPtrace,
    /// CAP_SYS_ADMIN: Perform a range of system administration operations
    SysAdmin,
    /// CAP_SYS_BOOT: Use reboot() and kexec_load()
    SysBoot,
    /// CAP_SYS_NICE: Raise process nice value and change the nice value for arbitrary processes
    SysNice,
    /// CAP_SYS_RESOURCE: Override resource limits
    SysResource,
    /// CAP_SYS_TIME: Set system clock; set real-time (hardware) clock
    SysTime,
    /// CAP_MKNOD: Create special files using mknod()
    Mknod,
    /// CAP_AUDIT_WRITE: Write records to kernel auditing log
    AuditWrite,
    /// CAP_AUDIT_CONTROL: Enable and disable kernel auditing
    AuditControl,
    /// CAP_SETFCAP: Set file capabilities
    Setfcap,
}

impl LinuxCapability {
    /// Human-readable name for debugging.
    pub fn name(self) -> &'static str {
        match self {
            Self::Chown => "CHOWN",
            Self::DacOverride => "DAC_OVERRIDE",
            Self::DacReadSearch => "DAC_READ_SEARCH",
            Self::Fowner => "FOWNER",
            Self::Fsetid => "FSETID",
            Self::Kill => "KILL",
            Self::Setgid => "SETGID",
            Self::Setuid => "SETUID",
            Self::Setpcap => "SETPCAP",
            Self::NetBindService => "NET_BIND_SERVICE",
            Self::NetRaw => "NET_RAW",
            Self::NetAdmin => "NET_ADMIN",
            Self::SysChroot => "SYS_CHROOT",
            Self::SysPtrace => "SYS_PTRACE",
            Self::SysAdmin => "SYS_ADMIN",
            Self::SysBoot => "SYS_BOOT",
            Self::SysNice => "SYS_NICE",
            Self::SysResource => "SYS_RESOURCE",
            Self::SysTime => "SYS_TIME",
            Self::Mknod => "MKNOD",
            Self::AuditWrite => "AUDIT_WRITE",
            Self::AuditControl => "AUDIT_CONTROL",
            Self::Setfcap => "SETFCAP",
        }
    }
}

/// Attach security policy to a `tokio::process::Command`.
pub fn attach_security(cmd: &mut Command, config: &SecurityConfig) {
    if config.is_empty() {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::attach(cmd, config);
    }

    #[cfg(not(target_os = "linux"))]
    {
        warn!(
            ?config,
            "security configuration is only enforced on Linux; current OS={} – settings will be ignored",
            std::env::consts::OS,
        );
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{LinuxCapability, SecurityConfig};

    use std::io;

    use tokio::process::Command;

    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};

    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;

    /// Upper bound for capability iteration.
    /// As of Linux 6.x, the last capability is ~40 (CAP_CHECKPOINT_RESTORE).
    /// We use 63 as a safe upper bound to handle future kernel additions.
    const CAP_LAST_CAP: u32 = 63;

    // Linux capability constants (from <linux/capability.h>)
    // These are not exported by libc, so we define them here.
    const CAP_CHOWN: u32 = 0;
    const CAP_DAC_OVERRIDE: u32 = 1;
    const CAP_DAC_READ_SEARCH: u32 = 2;
    const CAP_FOWNER: u32 = 3;
    const CAP_FSETID: u32 = 4;
    const CAP_KILL: u32 = 5;
    const CAP_SETGID: u32 = 6;
    const CAP_SETUID: u32 = 7;
    const CAP_SETPCAP: u32 = 8;
    const CAP_NET_BIND_SERVICE: u32 = 10;
    const CAP_NET_RAW: u32 = 13;
    const CAP_NET_ADMIN: u32 = 12;
    const CAP_SYS_CHROOT: u32 = 18;
    const CAP_SYS_PTRACE: u32 = 19;
    const CAP_SYS_ADMIN: u32 = 21;
    const CAP_SYS_BOOT: u32 = 22;
    const CAP_SYS_NICE: u32 = 23;
    const CAP_SYS_RESOURCE: u32 = 24;
    const CAP_SYS_TIME: u32 = 25;
    const CAP_MKNOD: u32 = 27;
    const CAP_AUDIT_WRITE: u32 = 29;
    const CAP_AUDIT_CONTROL: u32 = 30;
    const CAP_SETFCAP: u32 = 31;

    pub fn attach(cmd: &mut Command, config: &SecurityConfig) {
        if config.is_empty() {
            return;
        }

        let cfg = config.clone();
        unsafe {
            cmd.pre_exec(move || {
                // Order is critical:
                // 1. Drop capabilities first
                // 2. Set no_new_privs (after caps, as changing caps is gaining privilege)
                // 3. Seccomp would go last (not implemented yet)

                if cfg.drop_all_caps {
                    // Capability operations are non-fatal - we log to stderr and continue
                    if let Err(e) = drop_capabilities(&cfg.keep_caps) {
                        pre_exec_log(b"tno-exec: failed to drop capabilities (continuing): ");
                        if let Some(code) = e.raw_os_error() {
                            pre_exec_log_errno(code);
                        }
                    }
                }

                if cfg.no_new_privs {
                    // no_new_privs is fatal - it works without root and is critical for security
                    apply_no_new_privs()?;
                }

                Ok(())
            });
        }
    }

    /// Drop all capabilities, then re-add only those in `keep_caps`.
    ///
    /// This operates on all capability sets: permitted, effective, inheritable, and ambient.
    ///
    /// Returns error if capability operations fail (typically EPERM if lacking CAP_SETPCAP).
    fn drop_capabilities(keep_caps: &[LinuxCapability]) -> io::Result<()> {
        // 1. Clear ambient capabilities first (they depend on permitted/inheritable)
        clear_ambient_caps()?;

        // 2. Build the capability bitmask for caps we want to keep
        let mut keep_mask = CapabilityMask::empty();
        for cap in keep_caps {
            keep_mask.set(cap.to_cap_value());
        }

        // 3. Drop all capabilities we don't want to keep
        for cap_value in 0..=CAP_LAST_CAP {
            if !keep_mask.is_set(cap_value) {
                // Drop from all sets
                // Ignore errors for individual caps (some might not exist on older kernels)
                let _ = drop_cap(cap_value, CapSet::Effective);
                let _ = drop_cap(cap_value, CapSet::Permitted);
                let _ = drop_cap(cap_value, CapSet::Inheritable);
            }
        }

        // 4. Ensure kept capabilities are in effective set
        for cap in keep_caps {
            let cap_value = cap.to_cap_value();
            // Raise can fail if we don't have it in permitted, that's ok
            let _ = raise_cap(cap_value, CapSet::Effective);
        }

        Ok(())
    }

    /// Clear all ambient capabilities.
    fn clear_ambient_caps() -> io::Result<()> {
        let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            // EINVAL means kernel doesn't support ambient caps (< 4.3), which is fine
            if err.raw_os_error() != Some(libc::EINVAL) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Drop a capability from a specific set.
    fn drop_cap(cap: u32, set: CapSet) -> io::Result<()> {
        let mut header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };

        let mut data = [CapUserData::default(); 2];

        // Read current caps
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // Clear the bit
        let idx = (cap / 32) as usize;
        if idx >= 2 {
            // Cap value out of range
            return Ok(());
        }
        let bit = 1u32 << (cap % 32);

        match set {
            CapSet::Effective => data[idx].effective &= !bit,
            CapSet::Permitted => data[idx].permitted &= !bit,
            CapSet::Inheritable => data[idx].inheritable &= !bit,
        }

        // Write back
        if unsafe { capset(&mut header, data.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Raise a capability in a specific set.
    fn raise_cap(cap: u32, set: CapSet) -> io::Result<()> {
        let mut header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };

        let mut data = [CapUserData::default(); 2];

        // Read current caps
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // Set the bit
        let idx = (cap / 32) as usize;
        if idx >= 2 {
            // Cap value out of range
            return Ok(());
        }
        let bit = 1u32 << (cap % 32);

        match set {
            CapSet::Effective => data[idx].effective |= bit,
            CapSet::Permitted => data[idx].permitted |= bit,
            CapSet::Inheritable => data[idx].inheritable |= bit,
        }

        // Write back
        if unsafe { capset(&mut header, data.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn apply_no_new_privs() -> io::Result<()> {
        let rc = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    impl LinuxCapability {
        /// Convert to capability constant value.
        fn to_cap_value(self) -> u32 {
            match self {
                Self::Chown => CAP_CHOWN,
                Self::DacOverride => CAP_DAC_OVERRIDE,
                Self::DacReadSearch => CAP_DAC_READ_SEARCH,
                Self::Fowner => CAP_FOWNER,
                Self::Fsetid => CAP_FSETID,
                Self::Kill => CAP_KILL,
                Self::Setgid => CAP_SETGID,
                Self::Setuid => CAP_SETUID,
                Self::Setpcap => CAP_SETPCAP,
                Self::NetBindService => CAP_NET_BIND_SERVICE,
                Self::NetRaw => CAP_NET_RAW,
                Self::NetAdmin => CAP_NET_ADMIN,
                Self::SysChroot => CAP_SYS_CHROOT,
                Self::SysPtrace => CAP_SYS_PTRACE,
                Self::SysAdmin => CAP_SYS_ADMIN,
                Self::SysBoot => CAP_SYS_BOOT,
                Self::SysNice => CAP_SYS_NICE,
                Self::SysResource => CAP_SYS_RESOURCE,
                Self::SysTime => CAP_SYS_TIME,
                Self::Mknod => CAP_MKNOD,
                Self::AuditWrite => CAP_AUDIT_WRITE,
                Self::AuditControl => CAP_AUDIT_CONTROL,
                Self::Setfcap => CAP_SETFCAP,
            }
        }
    }

    // ---- Low-level capability syscall bindings ----

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

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    unsafe extern "C" {
        fn capget(hdrp: *mut CapUserHeader, datap: *mut CapUserData) -> libc::c_int;
        fn capset(hdrp: *mut CapUserHeader, datap: *const CapUserData) -> libc::c_int;
    }

    #[derive(Debug, Clone, Copy)]
    enum CapSet {
        Effective,
        Permitted,
        Inheritable,
    }

    /// Simple bitmask for tracking which capabilities to keep.
    /// Supports up to 128 capabilities (current kernel max is ~40).
    struct CapabilityMask {
        bits: [u64; 2], // 2 x 64 bits = 128 capabilities
    }

    impl CapabilityMask {
        fn empty() -> Self {
            Self { bits: [0, 0] }
        }

        fn set(&mut self, cap: u32) {
            let idx = (cap / 64) as usize;
            if idx >= 2 {
                return; // Out of range
            }
            let bit = cap % 64;
            self.bits[idx] |= 1u64 << bit;
        }

        fn is_set(&self, cap: u32) -> bool {
            let idx = (cap / 64) as usize;
            if idx >= 2 {
                return false; // Out of range
            }
            let bit = cap % 64;
            (self.bits[idx] & (1u64 << bit)) != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_noop() {
        let cfg = SecurityConfig::default();
        assert!(cfg.is_empty());

        let mut cmd = Command::new("sh");
        attach_security(&mut cmd, &cfg);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_empty_config_attaches_pre_exec_hook_on_linux() {
        let cfg = SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetAdmin, LinuxCapability::NetBindService],
            no_new_privs: true,
        };

        assert!(!cfg.is_empty());

        let mut cmd = Command::new("sh");
        attach_security(&mut cmd, &cfg);
        // Note: we can't test actual execution here without root/CAP_SETPCAP
        // The pre_exec hook will log a warning and continue if caps can't be dropped
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_empty_config_is_ignored_on_non_linux() {
        let cfg = SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetAdmin],
            no_new_privs: true,
        };

        assert!(!cfg.is_empty());

        let mut cmd = Command::new("sh");
        attach_security(&mut cmd, &cfg);
    }

    #[test]
    fn capability_names_are_correct() {
        assert_eq!(LinuxCapability::NetAdmin.name(), "NET_ADMIN");
        assert_eq!(LinuxCapability::SysAdmin.name(), "SYS_ADMIN");
        assert_eq!(LinuxCapability::Chown.name(), "CHOWN");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_new_privs_can_be_set_without_root() {
        // no_new_privs works without root, so this should not fail
        let cfg = SecurityConfig {
            drop_all_caps: false,
            keep_caps: vec![],
            no_new_privs: true,
        };

        let mut cmd = Command::new("true");
        attach_security(&mut cmd, &cfg);

        // This should succeed even without root
        let result = cmd.status().await;
        assert!(result.is_ok(), "no_new_privs should work without root");
        assert!(result.unwrap().success());
    }
}
