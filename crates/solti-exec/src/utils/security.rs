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
        let _ = &cmd;
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

        // Pre-compute a stack-local bitmask from keep_caps Vec — avoids cloning the Vec
        // into the closure (which would heap-allocate between closure creation and fork).
        let drop_all_caps = config.drop_all_caps;
        let no_new_privs = config.no_new_privs;
        let keep_mask = KeepMask::from_caps(&config.keep_caps);

        // SAFETY: The pre_exec closure runs between fork() and execve() in the child process.
        // It calls prctl, capget/capset (async-signal-safe syscalls) and pre_exec_log
        // (raw libc::write). Error paths use io::Error::last_os_error() which stores errno
        // inline without heap allocation (Rust >= 1.74). Capability drop failures are non-fatal
        // (logged and continued); no_new_privs failure is fatal (returns Err, aborting spawn).
        // The closure captures only Copy types (two bools + [u32; 2]) — zero heap allocation.
        unsafe {
            cmd.pre_exec(move || {
                if drop_all_caps && let Err(e) = drop_capabilities_batch(keep_mask) {
                    pre_exec_log(b"solti-exec: failed to drop capabilities (continuing): ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                }
                if no_new_privs {
                    apply_no_new_privs()?;
                }
                Ok(())
            });
        }
    }

    /// Drop all capabilities except those in `keep_mask`, using batch capget/capset.
    ///
    /// This performs exactly 1 capget + 1 capset + 1 prctl (clear ambient),
    /// plus optional ambient raises for kept caps — instead of the previous
    /// O(CAP_LAST_CAP × 3) individual syscall pairs.
    fn drop_capabilities_batch(keep_mask: KeepMask) -> io::Result<()> {
        clear_ambient_caps()?;

        let mut header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapUserData::default(); 2];

        // SAFETY: header and data are valid stack-local #[repr(C)] structs matching
        // the kernel's __user_cap_header_struct / __user_cap_data_struct layout.
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // Apply keep_mask: clear all bits not in the mask for all three sets.
        data[0].effective &= keep_mask.bits[0];
        data[0].permitted &= keep_mask.bits[0];
        data[0].inheritable &= keep_mask.bits[0];
        data[1].effective &= keep_mask.bits[1];
        data[1].permitted &= keep_mask.bits[1];
        data[1].inheritable &= keep_mask.bits[1];

        // SAFETY: Same structs, modified in-place. Single capset writes the new state.
        if unsafe { capset(&mut header, data.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }

        // Raise kept caps in ambient set (best-effort, may fail on older kernels).
        for cap_value in 0..=CAP_LAST_CAP {
            if keep_mask.is_set(cap_value) {
                let _ = raise_ambient_cap(cap_value);
            }
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

    /// Stack-only bitmask matching the kernel's capability v3 layout ([u32; 2]).
    /// Captures the keep-list without heap allocation (no Vec clone).
    #[derive(Clone, Copy)]
    struct KeepMask {
        /// bits[0] covers caps 0..31, bits[1] covers caps 32..63.
        bits: [u32; 2],
    }

    impl KeepMask {
        /// Build a keep-mask from a slice of capabilities.
        /// Called once before fork — safe to iterate a slice here.
        fn from_caps(caps: &[LinuxCapability]) -> Self {
            let mut bits = [0u32; 2];
            for cap in caps {
                let v = cap.to_cap_value();
                let idx = (v / 32) as usize;
                if idx < 2 {
                    bits[idx] |= 1u32 << (v % 32);
                }
            }
            Self { bits }
        }

        fn is_set(self, cap: u32) -> bool {
            let idx = (cap / 32) as usize;
            if idx >= 2 {
                return false;
            }
            (self.bits[idx] & (1u32 << (cap % 32))) != 0
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
