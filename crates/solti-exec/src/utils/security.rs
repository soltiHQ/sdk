//! Basic security hardening for subprocess-based runners.
//!
//! ## Overview
//!
//! This module provides API for configuring process-level security to child processes created via `tokio::process::Command`.
//! - On **Linux platforms** security settings are applied inside a `pre_exec` hook.
//! - On **non-Linux platforms**, limits are ignored: a warning is emitted and the call returns `Ok(())`.
use tokio::process::Command;

use crate::utils::LinuxCapability;

#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// Declarative security policy.
#[derive(Debug, Clone, Default)]
pub struct SecurityConfig {
    /// Drop all capabilities before exec.
    ///
    /// Note: capability operations require CAP_SETPCAP or root.
    /// If the process lacks these privileges, the operation will log a warning and continue (non-fatal).
    pub drop_all_caps: bool,
    /// Optional allowlist of capabilities to keep after `drop_all_caps`.
    ///
    /// Only meaningful when `drop_all_caps = true`.
    pub keep_caps: Vec<LinuxCapability>,
    /// Enable `no_new_privs` for the child process.
    ///
    /// This flag works without root privileges.
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
    use super::SecurityConfig;
    use crate::utils::{
        LinuxCapability,
        log::{pre_exec_log, pre_exec_log_errno},
    };

    use std::io;

    use tokio::process::Command;

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    const CAP_LAST_CAP: u32 = 63;

    pub fn attach(cmd: &mut Command, config: &SecurityConfig) {
        if config.is_empty() {
            return;
        }

        let cfg = config.clone();
        unsafe {
            cmd.pre_exec(move || {
                if cfg.drop_all_caps
                    && let Err(e) = drop_capabilities(&cfg.keep_caps)
                {
                    pre_exec_log(b"solti-exec: failed to drop capabilities (continuing): ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                }
                if cfg.no_new_privs {
                    apply_no_new_privs()?;
                }
                Ok(())
            });
        }
    }

    /// Drop all capabilities, then re-add only those in `keep_caps`.
    ///
    /// This operates on all capability sets: permitted, effective, inheritable, and ambient.
    fn drop_capabilities(keep_caps: &[LinuxCapability]) -> io::Result<()> {
        clear_ambient_caps()?;

        let mut keep_mask = CapabilityMask::empty();
        for cap in keep_caps {
            keep_mask.set(cap.to_cap_value());
        }
        for cap_value in 0..=CAP_LAST_CAP {
            if !keep_mask.is_set(cap_value) {
                let _ = drop_cap(cap_value, CapSet::Effective);
                let _ = drop_cap(cap_value, CapSet::Permitted);
                let _ = drop_cap(cap_value, CapSet::Inheritable);
            }
        }
        for cap in keep_caps {
            let cap_value = cap.to_cap_value();
            let _ = raise_cap(cap_value, CapSet::Effective);
        }
        for cap in keep_caps {
            let cap_value = cap.to_cap_value();

            // Only raise in ambient if it's in permitted and inheritable
            // We ignore errors here - ambient might not be supported on older kernels,
            // or the cap might not be in the required sets
            let _ = raise_ambient_cap(cap_value);
        }
        Ok(())
    }

    /// Clear all ambient capabilities.
    fn clear_ambient_caps() -> io::Result<()> {
        let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINVAL) {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Raise a capability in the ambient set.
    ///
    /// Returns `Ok(())` even if the operation fails.
    /// Failures can happen on:
    /// - Kernel < 4.3 (no ambient caps support)
    /// - Cap not in permitted+inheritable
    /// - EPERM if lacking CAP_SETPCAP
    fn raise_ambient_cap(cap: u32) -> io::Result<()> {
        let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, cap, 0, 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINVAL) | Some(libc::EPERM) => return Ok(()),
                _ => return Err(err),
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
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let idx = (cap / 32) as usize;
        if idx >= 2 {
            return Ok(());
        }
        let bit = 1u32 << (cap % 32);

        match set {
            CapSet::Effective => data[idx].effective &= !bit,
            CapSet::Permitted => data[idx].permitted &= !bit,
            CapSet::Inheritable => data[idx].inheritable &= !bit,
        }
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
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let idx = (cap / 32) as usize;
        if idx >= 2 {
            return Ok(());
        }
        let bit = 1u32 << (cap % 32);

        match set {
            CapSet::Effective => data[idx].effective |= bit,
            CapSet::Permitted => data[idx].permitted |= bit,
            CapSet::Inheritable => data[idx].inheritable |= bit,
        }
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
        fn capget(hdrp: *mut CapUserHeader, datap: *mut CapUserData) -> libc::c_int;
        fn capset(hdrp: *mut CapUserHeader, datap: *const CapUserData) -> libc::c_int;
    }

    #[derive(Debug, Clone, Copy)]
    enum CapSet {
        Effective,
        Permitted,
        Inheritable,
    }

    struct CapabilityMask {
        bits: [u64; 2],
    }

    impl CapabilityMask {
        fn empty() -> Self {
            Self { bits: [0, 0] }
        }

        fn set(&mut self, cap: u32) {
            let idx = (cap / 64) as usize;
            if idx >= 2 {
                return;
            }
            let bit = cap % 64;
            self.bits[idx] |= 1u64 << bit;
        }

        fn is_set(&self, cap: u32) -> bool {
            let idx = (cap / 64) as usize;
            if idx >= 2 {
                return false;
            }
            let bit = cap % 64;
            (self.bits[idx] & (1u64 << bit)) != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

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
        let cfg = SecurityConfig {
            drop_all_caps: false,
            keep_caps: vec![],
            no_new_privs: true,
        };
        let mut cmd = Command::new("true");
        attach_security(&mut cmd, &cfg);

        let result = cmd.status().await;
        assert!(result.is_ok(), "no_new_privs should work without root");
        assert!(result.unwrap().success());
    }
}
