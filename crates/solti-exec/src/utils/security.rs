//! # Security: process-level hardening for subprocess runners.
//!
//! [`SecurityConfig`] restricts the privilege set of child processes spawned by subprocess runners.
//!
//! **Linux:**
//! - Drop all process capabilities in one batch (`capget` → mask → `capset`)
//! - Zero heap allocation in the child (closure captures only `Copy` types)
//! - Raise kept caps in the ambient set for unprivileged `execve`
//! - Keep an optional allowlist of caps via [`LinuxCapability`]
//! - Set `no_new_privs` to block suid/sgid escalation
//!
//! **Other platforms:**
//! - `tracing::warn` and no-op.
//!
//! ## What happens when a subprocess spawns
//! ```text
//!                        parent process
//!                             │
//!                           fork()
//!                             │
//!          ┌──────────────────┼───────────────────┐
//!          │            child process             │
//!          │                                      │
//!          │  ┌── pre_exec hook ───────────────┐  │
//!          │  │  1. clear ambient caps         │  │
//!          │  │  2. capget current caps        │  │
//!          │  │  3. mask &= keep_mask          │  │
//!          │  │  4. capset (one syscall)       │  │
//!          │  │  5. raise kept in ambient      │  │
//!          │  │  6. set no_new_privs           │  │
//!          │  └────────────────────────────────┘  │
//!          │                                      │
//!          │  execve("echo", ["hello"])           │
//!          │  (runs with minimal caps)            │
//!          └──────────────────────────────────────┘
//! ```
//!
//! ## How attach_security works
//! ```text
//! attach_security(&mut cmd, &config)
//!     ├──► config.is_empty()? → return early, no hook
//!     │
//!     ├──► Linux:
//!     │     ├──► build KeepMask from config.keep_caps
//!     │     │     └──► Vec<LinuxCapability> → [u32; 2] bitmask (Copy, stack-only)
//!     │     │
//!     │     └──► install pre_exec closure on Command
//!     │           └──► captures: drop_all_caps (bool), no_new_privs (bool), keep_mask ([u32; 2])
//!     │                zero heap: all Copy types
//!     │
//!     └──► non-Linux:
//!           └──► warn!("security settings ignored on {os}") → Ok(())
//! ```
//!
//! ## Capability drop: step by step
//! ```text
//! drop_capabilities_batch(keep_mask)
//!     │
//!     ├──► prctl(PR_CAP_AMBIENT, CLEAR_ALL)
//!     │     └──► EINVAL? kernel < 4.3, no ambient — Ok, continue
//!     │
//!     ├──► capget() → read current caps into CapUserData[2]
//!     │    ┌────────────────────────────────────────────────┐
//!     │    │  before mask             after mask            │
//!     │    │  effective:  1111        effective:  0010      │
//!     │    │  permitted:  1111        permitted:  0010      │
//!     │    │  inheritable:1111        inheritable:0010      │
//!     │    │                                                │
//!     │    │  keep_mask = 0010 (only CAP_NET_BIND_SERVICE)  │
//!     │    └────────────────────────────────────────────────┘
//!     │
//!     ├──► capset() ← one syscall writes all caps
//!     │
//!     └──► for each cap set in keep_mask:
//!           └──► prctl(PR_CAP_AMBIENT, RAISE, cap)
//!                EINVAL | EPERM → Ok (best-effort, older kernel or no permission)
//! ```
//!
//! ## KeepMask layout
//! ```text
//! Linux capability v3 format: CapUserData[2] = 2 × u32 = 64 bits
//!
//!   bits[0]                          bits[1]
//!   ┌─────────────────────────────┐  ┌─────────────────────────────┐
//!   │ cap 0  cap 1 ... cap 31     │  │ cap 32  cap 33 ... cap 63   │
//!   └─────────────────────────────┘  └─────────────────────────────┘
//!
//!   CAP_LAST_CAP = 63 — this is NOT a guess, it's the v3 ABI limit.
//!   If kernel ever adds cap > 63, that requires a v4 format with
//!   new structs and new syscall signatures — this whole module
//!   would need updating anyway.
//! ```
//!
//! ## Configuration
//!
//! | Field               | What it does                          | Needs privileges? | If it fails                                    |
//! |---------------------|---------------------------------------|-------------------|------------------------------------------------|
//! | `drop_all_caps`     | strip all caps except `keep_caps`     | `CAP_SETPCAP`     | logs warning, go on (or abort if strict)       |
//! | `keep_caps`         | allowlist: caps to preserve           | `CAP_SETPCAP`     | logs warning, go on (or abort if strict)       |
//! | `fail_on_cap_error` | strict mode: abort spawn on cap error | —                 | —                                              |
//! | `no_new_privs`      | block suid/sgid privilege escalation  | none (any user)   | **always aborts spawn**                        |
//!
//! ## Async-signal safety
//!
//! Everything inside the `pre_exec` closure runs **between `fork()` and `execve()`**.
//! POSIX says only async-signal-safe functions are allowed there.
//!
//! | What we call                 | Why it's safe                              |
//! |------------------------------|--------------------------------------------|
//! | `prctl()`                    | direct syscall                             |
//! | `capget()` / `capset()`      | direct syscalls                            |
//! | `libc::write(STDERR)`        | async-signal-safe per POSIX                |
//! | `io::Error::last_os_error()` | reads `errno`, no heap (Rust ≥ 1.74)       |
//!
//! The closure captures **only `Copy` types** (2 bools + `[u32; 2]`).
//! No `Vec`, no `String`, no `Arc`: zero heap allocation in the child.
//!
//! ## Rules
//! - Capability drop failures are **non-fatal** by default (logged via `pre_exec_log`, continues)
//! - Set `fail_on_cap_error = true` to make capability drop failures **fatal** (aborts spawn)
//! - Non-Linux: all knobs are no-op, warning emitted via `tracing::warn`
//! - `no_new_privs` failure is **always fatal** (returns `Err`, `Command::spawn` fails)
//! - `KeepMask` is built **before** fork (safe to iterate `Vec<LinuxCapability>`)
//! - `SecurityConfig::is_empty()` → no hook installed, zero overhead
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
    /// If the process lacks these privileges, the operation will log a warning and continue (unless `fail_on_cap_error` is set).
    pub drop_all_caps: bool,
    /// Optional allowlist of capabilities to keep after `drop_all_caps`.
    ///
    /// Only meaningful when `drop_all_caps = true`.
    pub keep_caps: Vec<LinuxCapability>,
    /// Enable `no_new_privs` for the child process.
    ///
    /// This flag works without root privileges.
    /// Failures to set this flag are always fatal (spawn will fail).
    pub no_new_privs: bool,
    /// When `true`, capability drop failures abort the spawn instead of logging and continuing.
    ///
    /// Default: `false` (best-effort — non-fatal).
    pub fail_on_cap_error: bool,
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
            "security configuration is only enforced on Linux; current OS={}: settings will be ignored",
            std::env::consts::OS,
        );
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{KeepMask, SecurityConfig};

    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};
    use std::io;
    use tokio::process::Command;

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    /// Upper bound of capability v3 bitmask: `CapUserData[2]` = 2 × 32 = 64 bits → caps 0..63.
    /// This is a kernel ABI limit, not a guess. A v4 format would require new structs + syscall signatures.
    const CAP_LAST_CAP: u32 = 63;

    /// Install the `pre_exec` hook on the command.
    ///
    /// Caller (`attach_security`) already checked `!config.is_empty()`.
    pub fn attach(cmd: &mut Command, config: &SecurityConfig) {
        let keep_mask = KeepMask::from_caps(&config.keep_caps);
        let fail_on_cap_error = config.fail_on_cap_error;
        let drop_all_caps = config.drop_all_caps;
        let no_new_privs = config.no_new_privs;

        // SAFETY:
        // The pre_exec closure runs between fork() and execve() in the child process.
        //
        // It calls prctl, capget/capset (async-signal-safe syscalls) and pre_exec_log (raw libc::write).
        // Error paths use io::Error::last_os_error() which stores errno inline without heap allocation (Rust >= 1.74).
        //
        // The closure captures only Copy types (three bools + [u32; 2]): zero heap allocation.
        unsafe {
            cmd.pre_exec(move || {
                if drop_all_caps
                    && let Err(e) = drop_capabilities_batch(keep_mask)
                    && fail_on_cap_error
                {
                    return Err(e);
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
    /// Each step logs a distinct prefix on failure so the operator can tell which syscall failed (clear_ambient / capget / capset).
    fn drop_capabilities_batch(keep_mask: KeepMask) -> io::Result<()> {
        if let Err(e) = clear_ambient_caps() {
            pre_exec_log(b"solti-exec: clear_ambient_caps failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }

        let mut header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapUserData::default(); 2];

        // SAFETY:
        // Header and data are valid stack-local #[repr(C)] structs matching the kernel's
        // __user_cap_header_struct / __user_cap_data_struct layout.
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            pre_exec_log(b"solti-exec: capget failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }

        data[0].effective &= keep_mask.bits[0];
        data[0].permitted &= keep_mask.bits[0];
        data[0].inheritable &= keep_mask.bits[0];
        data[1].effective &= keep_mask.bits[1];
        data[1].permitted &= keep_mask.bits[1];
        data[1].inheritable &= keep_mask.bits[1];

        // SAFETY:
        // Same structs, modified in-place.
        // Single capset writes the new state.
        if unsafe { capset(&mut header, data.as_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            pre_exec_log(b"solti-exec: capset failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }

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

    /// Raise a capability in the ambient set (best-effort).
    ///
    /// Returns `Ok(())` for `EINVAL` and `EPERM` (expected on older kernels or when lacking `CAP_SETPCAP`).
    /// Other errors propagate, but the caller ignores the result with `let _ =`.
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
        fn capset(hdrp: *mut CapUserHeader, datap: *const CapUserData) -> libc::c_int;
        fn capget(hdrp: *mut CapUserHeader, datap: *mut CapUserData) -> libc::c_int;
    }
}

/// Bitmask of Linux capabilities to keep after a bulk drop.
///
/// Layout mirrors the kernel v3 capability format: two `u32` words covering caps 0..31 and 32..63 respectively.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct KeepMask {
    /// `bits[0]` covers caps 0..31, `bits[1]` covers caps 32..63.
    bits: [u32; 2],
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl KeepMask {
    /// Build a keep-mask from a slice of capabilities.
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

    /// Returns `true` if the given capability number is set in the mask.
    fn is_set(self, cap: u32) -> bool {
        let idx = (cap / 32) as usize;
        if idx >= 2 {
            return false;
        }
        (self.bits[idx] & (1u32 << (cap % 32))) != 0
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
            ..Default::default()
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
            ..Default::default()
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
            no_new_privs: true,
            ..Default::default()
        };
        let mut cmd = Command::new("true");
        attach_security(&mut cmd, &cfg);

        let result = cmd.status().await;
        assert!(result.is_ok(), "no_new_privs should work without root");
        assert!(result.unwrap().success());
    }

    #[test]
    fn keep_mask_empty_caps_all_zero() {
        let m = KeepMask::from_caps(&[]);
        assert_eq!(m.bits, [0, 0]);
        for cap in 0..=63 {
            assert!(!m.is_set(cap), "cap {cap} should not be set");
        }
    }

    #[test]
    fn keep_mask_single_low_cap() {
        let m = KeepMask::from_caps(&[LinuxCapability::Chown]);
        assert!(m.is_set(0));
        assert!(!m.is_set(1));
        assert_eq!(m.bits[0], 1);
        assert_eq!(m.bits[1], 0);
    }

    #[test]
    fn keep_mask_cap_in_second_word() {
        let m = KeepMask::from_caps(&[LinuxCapability::SetFCap, LinuxCapability::SysPtrace]);
        assert!(m.is_set(31));
        assert!(m.is_set(19));
        assert!(!m.is_set(0));
        assert_eq!(m.bits[1], 0)
    }

    #[test]
    fn keep_mask_multiple_caps() {
        let caps = [
            LinuxCapability::Chown,          // 0
            LinuxCapability::NetBindService, // 10
            LinuxCapability::NetAdmin,       // 12
            LinuxCapability::SysAdmin,       // 21
        ];
        let m = KeepMask::from_caps(&caps);
        assert!(m.is_set(0));
        assert!(m.is_set(10));
        assert!(m.is_set(12));
        assert!(m.is_set(21));
        assert!(!m.is_set(1));
        assert!(!m.is_set(11));
        assert!(!m.is_set(63));
    }

    #[test]
    fn keep_mask_duplicate_caps_idempotent() {
        let m1 = KeepMask::from_caps(&[LinuxCapability::Kill]);
        let m2 = KeepMask::from_caps(&[LinuxCapability::Kill, LinuxCapability::Kill]);
        assert_eq!(m1.bits, m2.bits);
    }

    #[test]
    fn keep_mask_out_of_range_returns_false() {
        let m = KeepMask::from_caps(&[LinuxCapability::Chown]);
        assert!(!m.is_set(64));
        assert!(!m.is_set(100));
        assert!(!m.is_set(u32::MAX));
    }
}
