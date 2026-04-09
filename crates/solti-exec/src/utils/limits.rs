//! # Limits: POSIX rlimit-based resource limits for subprocess runners.
//!
//! [`RlimitConfig`] applies classic POSIX process limits to child processes spawned by subprocess runners.
//!
//! **Unix:**
//! - Limits are applied inside a `pre_exec` hook (between `fork()` and `execve()`)
//! - Each limit uses `getrlimit` → clamp → `setrlimit` (two syscalls)
//! - Hard limit is never touched — only the soft limit is lowered/raised within bounds
//! - Zero heap allocation in the child (closure captures only `Copy` types)
//!
//! **Other platforms:** `tracing::warn` and no-op.
//!
//! ## Also
//!
//! - [`SubprocessBackendConfig`](crate::subprocess::SubprocessBackendConfig) builder that consumes `RlimitConfig`.
//! - [`CgroupLimits`](super::CgroupLimits) complementary cgroup v2 limits.
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
//!          │  │  for each configured limit:    │  │
//!          │  │    1. getrlimit(resource)      │  │
//!          │  │    2. clamp value ≤ hard limit │  │
//!          │  │    3. setrlimit(soft, hard)    │  │
//!          │  └────────────────────────────────┘  │
//!          │                                      │
//!          │  execve("echo", ["hello"])           │
//!          │  (runs with restricted limits)       │
//!          └──────────────────────────────────────┘
//! ```
//!
//! ## How attach_rlimits works
//! ```text
//! attach_rlimits(&mut cmd, &config)
//!     ├──► config.is_empty()? → return early, no hook
//!     │
//!     ├──► Unix:
//!     │     └──► install pre_exec closure on Command
//!     │           └──► captures: 2 × Option<u64> + 1 × bool (all Copy)
//!     │
//!     │           pre_exec:
//!     │           ├──► max_open_files → apply_rlimit(NOFILE, value)
//!     │           ├──► max_file_size_bytes → apply_rlimit(FSIZE, value)
//!     │           └──► disable_core_dumps → apply_rlimit(CORE, 0)
//!     │
//!     └──► non-Unix:
//!           └──► warn!("rlimits ignored on {os}")
//! ```
//!
//! ## apply_rlimit: soft limit clamping
//! ```text
//! apply_rlimit(resource, requested_value)
//!     │
//!     ├──► getrlimit() → read current { soft, hard }
//!     │
//!     ├──► hard == INFINITY?
//!     │     └──► new_soft = requested (no ceiling)
//!     │
//!     ├──► requested > hard?
//!     │     └──► new_soft = hard (clamp — can't exceed hard without root)
//!     │
//!     ├──► requested ≤ hard?
//!     │     └──► new_soft = requested
//!     │
//!     └──► setrlimit(new_soft, hard)  ← hard is NEVER modified
//! ```
//!
//! ## Configuration
//!
//! | Field                  | Resource        | If it fails      |
//! |------------------------|-----------------|------------------|
//! | `max_open_files`       | `RLIMIT_NOFILE` | **aborts spawn** |
//! | `max_file_size_bytes`  | `RLIMIT_FSIZE`  | **aborts spawn** |
//! | `disable_core_dumps`   | `RLIMIT_CORE`   | **aborts spawn** |
//!
//! ## Async-signal safety
//!
//! Everything inside the `pre_exec` closure runs **between `fork()` and `execve()`**.
//!
//! | What we call                 | Why it's safe                              |
//! |------------------------------|--------------------------------------------|
//! | `getrlimit()` / `setrlimit()`| direct syscalls                            |
//! | `libc::write(STDERR)`        | async-signal-safe per POSIX                |
//! | `io::Error::last_os_error()` | reads `errno`, no heap (Rust ≥ 1.74)       |
//!
//! The closure captures **only `Copy` types** (2 × `Option<u64>` + 1 × `bool`).
//!
//! ## Rules
//! - Requested value exceeding hard limit is **silently clamped** (not an error)
//! - Non-Unix: all knobs are no-op, warning emitted via `tracing::warn`
//! - All rlimit failures are **fatal** (return `Err`, aborting spawn)
//! - Hard limit is **never modified** — only the soft limit changes
//! - `RlimitConfig::is_empty()` → no hook installed, zero overhead
use tokio::process::Command;

#[cfg(not(unix))]
use tracing::warn;

/// Declarative rlimit-based config.
#[derive(Debug, Clone, Default)]
pub struct RlimitConfig {
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`).
    ///
    /// Typical values:
    /// - `Some(1024)` for "normal" processes
    /// - `Some(4096)`/`8192` for IO-heavy tasks
    /// - `None` leaves the OS / parent limits unchanged.
    pub max_open_files: Option<u64>,
    /// Maximum size of created files in bytes (`RLIMIT_FSIZE`).
    ///
    /// When the process attempts to grow a file beyond this limit, the kernel typically delivers `SIGXFSZ` and the process terminates.
    /// `None` leaves the OS / parent limits unchanged.
    pub max_file_size_bytes: Option<u64>,
    /// Disable core dumps (`RLIMIT_CORE = 0`) when set to `true`.
    ///
    /// This prevents large core files from being written for failing tasks.
    /// When `false`, the OS default / inherited core limit is preserved.
    pub disable_core_dumps: bool,
}

impl RlimitConfig {
    /// Returns `true` if no explicit limits are configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.max_open_files.is_none()
            && self.max_file_size_bytes.is_none()
            && !self.disable_core_dumps
    }
}

/// Attach `rlimit`-based process limits to a `tokio::process::Command`.
pub fn attach_rlimits(cmd: &mut Command, config: &RlimitConfig) {
    if config.is_empty() {
        return;
    }

    #[cfg(unix)]
    {
        unix_impl::attach_rlimits(cmd, config);
    }
    #[cfg(not(unix))]
    {
        warn!(
            ?config,
            "rlimit-based process limits requested on a non-Unix OS; limits will be ignored"
        );
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::RlimitConfig;
    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};

    use std::io;

    use tokio::process::Command;

    /// Caller (`attach_rlimits`) already checked `!config.is_empty()`.
    pub fn attach_rlimits(cmd: &mut Command, config: &RlimitConfig) {
        let max_file_size_bytes = config.max_file_size_bytes;
        let disable_core_dumps = config.disable_core_dumps;
        let max_open_files = config.max_open_files;

        // SAFETY:
        // The pre_exec closure runs between fork() and execve() in the child process.
        // It only calls setrlimit/getrlimit (async-signal-safe syscalls) and pre_exec_log (raw libc::write to stderr).
        // Error paths use io::Error::last_os_error() which stores errno inline without heap allocation (Rust >= 1.74).
        unsafe {
            cmd.pre_exec(move || {
                if let Some(nofile) = max_open_files
                    && let Err(e) = apply_rlimit(NOFILE, nofile)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_NOFILE: ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(e);
                }
                if let Some(fsize) = max_file_size_bytes
                    && let Err(e) = apply_rlimit(FSIZE, fsize)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_FSIZE: ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(e);
                }
                if disable_core_dumps && let Err(e) = apply_rlimit(CORE, 0) {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_CORE: ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(e);
                }
                Ok(())
            });
        }
    }

    /// Resource type accepted by `getrlimit`/`setrlimit`.
    ///
    /// On Linux/Android it's `__rlimit_resource_t` (enum), elsewhere `c_int`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    type RlimitResource = libc::__rlimit_resource_t;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type RlimitResource = libc::c_int;

    const NOFILE: RlimitResource = libc::RLIMIT_NOFILE as RlimitResource;
    const FSIZE: RlimitResource = libc::RLIMIT_FSIZE as RlimitResource;
    const CORE: RlimitResource = libc::RLIMIT_CORE as RlimitResource;

    /// Apply rlimit: set the soft limit to `value`, keep the hard limit unchanged.
    ///
    /// If `value` exceeds the current hard limit (and hard != INFINITY), the soft limit is clamped to the hard limit
    /// an unprivileged process cannot raise its own hard limit.
    fn apply_rlimit(resource: RlimitResource, value: u64) -> io::Result<()> {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        // SAFETY:
        // `current` is a valid stack-local rlimit struct, passed by pointer.
        if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let requested = value as libc::rlim_t;

        // Clamp to hard limit: unprivileged processes cannot raise it.
        let new_soft = if current.rlim_max == libc::RLIM_INFINITY {
            requested
        } else if requested > current.rlim_max {
            current.rlim_max
        } else {
            requested
        };

        let rlim = libc::rlimit {
            rlim_cur: new_soft,
            rlim_max: current.rlim_max,
        };

        // SAFETY:
        // `rlim` is a valid stack-local rlimit struct, passed by pointer.
        if unsafe { libc::setrlimit(resource, &rlim) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_noop() {
        let config = RlimitConfig::default();
        assert!(config.is_empty());

        let mut cmd = Command::new("sh");
        attach_rlimits(&mut cmd, &config);
    }

    #[cfg(unix)]
    #[test]
    fn non_empty_config_attaches_pre_exec_hook() {
        let config = RlimitConfig {
            max_open_files: Some(1024),
            max_file_size_bytes: Some(10 * 1024 * 1024),
            disable_core_dumps: true,
        };

        let mut cmd = Command::new("sh");
        attach_rlimits(&mut cmd, &config);
    }

    #[cfg(not(unix))]
    #[test]
    fn non_empty_config_is_ignored_on_non_unix() {
        let config = RlimitConfig {
            max_open_files: Some(512),
            max_file_size_bytes: None,
            disable_core_dumps: true,
        };

        let mut cmd = Command::new("sh");
        attach_rlimits(&mut cmd, &config);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rlimits_can_be_applied() {
        let config = RlimitConfig {
            max_open_files: Some(512),
            max_file_size_bytes: Some(1024 * 1024),
            disable_core_dumps: true,
        };

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("ulimit -a");
        attach_rlimits(&mut cmd, &config);

        let result = cmd.status().await;
        assert!(result.is_ok(), "rlimits should be applied successfully");
        assert!(result.unwrap().success());
    }
}
