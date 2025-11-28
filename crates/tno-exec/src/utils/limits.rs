//! POSIX rlimit-based resource limits for subprocess-based runners.
//!
//! ## Overview
//!
//! This module provides a structured and portable API for configuring classic POSIX process limits (`rlimit`) on child processes spawned via `tokio::process::Command`.
//! - On **Unix platforms** (`Linux`, `macOS`, `*BSD`):
//!   limits are applied inside a `pre_exec` hook, executed in the child process after `fork()` and immediately before `execve()`.
//!   This guarantees that the process never runs without the intended restrictions.
//! - On **non-Unix platforms**, rlimits are not supported.
//!   The module emits a warning and treats the request as a no-op, keeping the API consistent and allowing cross-platform execution without failing early.
use tokio::process::Command;
use tracing::warn;

/// Declarative rlimit-based for a child process.
///
/// All fields are optional:
/// - `None` means "no explicit limit" for that resource.
/// - `disable_core_dumps = false` keeps core dumps enabled (subject to OS defaults).
#[derive(Debug, Clone, Default)]
pub struct RlimitConfig {
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`).
    ///
    /// Typical values:
    /// - `Some(1024)` for "normal" processes
    /// - `Some(4096)`/`8192` for IO-heavy tasks
    ///
    /// `None` leaves the OS / parent limits unchanged.
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
            && !self.disable_core_dumps
            && self.max_file_size_bytes.is_none()
    }
}

/// Attach `rlimit`-based process limits to a `tokio::process::Command`.
///
/// On Unix:
/// - installs a `pre_exec` hook that calls `setrlimit` in the child process before `execve`.
/// On non-Unix:
/// - logs a warning if `config` is non-empty and does nothing.
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
            target: "tno_exec::limits",
            ?config,
            "rlimit-based process limits requested on a non-Unix OS; limits will be ignored"
        );
    }
}

#[cfg(unix)]
mod unix_impl {
    use super::RlimitConfig;

    use std::io;

    use libc;
    use tokio::process::Command;

    pub fn attach_rlimits(cmd: &mut Command, config: &RlimitConfig) {
        if config.is_empty() {
            return;
        }

        let max_file_size_bytes = config.max_file_size_bytes;
        let max_open_files = config.max_open_files;
        let disable_core_dumps = config.disable_core_dumps;

        unsafe {
            cmd.pre_exec(move || {
                if let Some(nofile) = max_open_files {
                    apply_rlimit(libc::RLIMIT_NOFILE, nofile)?;
                }
                if let Some(fsize) = max_file_size_bytes {
                    apply_rlimit(libc::RLIMIT_FSIZE, fsize)?;
                }
                if disable_core_dumps {
                    let rlim = libc::rlimit {
                        rlim_cur: 0 as libc::rlim_t,
                        rlim_max: 0 as libc::rlim_t,
                    };
                    if libc::setrlimit(libc::RLIMIT_CORE, &rlim) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    fn apply_rlimit(resource: libc::c_int, value: u64) -> io::Result<()> {
        let rlim = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value as libc::rlim_t,
        };

        let rc = unsafe { libc::setrlimit(resource, &rlim) };
        if rc != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_noop() {
        let config = RlimitConfig::default();
        assert!(config.is_empty());

        let mut cmd = Command::new("sh");
        // Should not panic or log anything special
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

        // We cannot easily assert rlimits from the parent, but this at least
        // verifies that the function does not panic on Unix and that the API
        // is callable with a non-empty config.
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
        // On non-Unix this should just log a warning and do nothing.
        attach_rlimits(&mut cmd, &config);
    }
}
