//! POSIX rlimit-based resource limits for subprocess-based runners.
//!
//! ## Overview
//!
//! This module provides API for configuring classic POSIX process limits (`rlimit`) to child processes created via `tokio::process::Command`.
//! - On **Unix platforms** limits are applied inside a `pre_exec` hook, executed in the child process after `fork()` and immediately before `execve()`.
//! - On **non-Unix platforms**, rlimits are ignored: a warning is emitted and the call returns `Ok(())`.
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

    pub fn attach_rlimits(cmd: &mut Command, config: &RlimitConfig) {
        if config.is_empty() {
            return;
        }

        let max_file_size_bytes = config.max_file_size_bytes;
        let max_open_files = config.max_open_files;
        let disable_core_dumps = config.disable_core_dumps;

        unsafe {
            cmd.pre_exec(move || {
                if let Some(nofile) = max_open_files
                    && let Err(e) = apply_rlimit(rlimit_nofile(), nofile)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_NOFILE: ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(e);
                }
                if let Some(fsize) = max_file_size_bytes
                    && let Err(e) = apply_rlimit(rlimit_fsize(), fsize)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_FSIZE: ");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(e);
                }
                if disable_core_dumps && let Err(e) = apply_rlimit(rlimit_core(), 0) {
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

    #[inline]
    fn rlimit_nofile() -> libc::c_int {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            libc::RLIMIT_NOFILE as libc::c_int
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            libc::RLIMIT_NOFILE
        }
    }

    #[inline]
    fn rlimit_fsize() -> libc::c_int {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            libc::RLIMIT_FSIZE as libc::c_int
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            libc::RLIMIT_FSIZE
        }
    }

    #[inline]
    fn rlimit_core() -> libc::c_int {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            libc::RLIMIT_CORE as libc::c_int
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            libc::RLIMIT_CORE
        }
    }

    /// Apply rlimit, preserving the hard limit if it's already higher.
    fn apply_rlimit(resource: libc::c_int, value: u64) -> io::Result<()> {
        let max_rlim = libc::rlim_t::MAX;
        if value > max_rlim {
            pre_exec_log(b"solti-exec: rlimit value exceeds platform maximum\n");
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rlimit value exceeds platform maximum",
            ));
        }
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { getrlimit_compat(resource, &mut current) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let new_soft = value as libc::rlim_t;
        let new_hard = if current.rlim_max == libc::RLIM_INFINITY {
            libc::RLIM_INFINITY
        } else if current.rlim_max > new_soft {
            current.rlim_max
        } else {
            new_soft
        };
        let rlim = libc::rlimit {
            rlim_cur: new_soft,
            rlim_max: new_hard,
        };

        if unsafe { setrlimit_compat(resource, &rlim) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Compatibility shim for getrlimit.
    #[inline]
    unsafe fn getrlimit_compat(resource: libc::c_int, rlim: *mut libc::rlimit) -> libc::c_int {
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::getrlimit(resource as libc::__rlimit_resource_t, rlim) }
        }
        #[cfg(not(target_os = "linux"))]
        {
            unsafe { libc::getrlimit(resource, rlim) }
        }
    }

    /// Compatibility shim for setrlimit.
    #[inline]
    unsafe fn setrlimit_compat(resource: libc::c_int, rlim: *const libc::rlimit) -> libc::c_int {
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::setrlimit(resource as libc::__rlimit_resource_t, rlim) }
        }
        #[cfg(not(target_os = "linux"))]
        {
            unsafe { libc::setrlimit(resource, rlim) }
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
