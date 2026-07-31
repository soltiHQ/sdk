//! # POSIX resource limits
//!
//! [`RlimitConfig`] sets soft process limits on Unix.
//!
//! ## Flow
//!
//! ```text
//! RlimitConfig
//!      │ pre_exec
//!      ▼
//! read current hard limit
//!      ▼
//! clamp requested soft limit
//!      ▼
//! execve with updated limit
//! ```
//!
//! The hard limit is never changed.
//! A request above it is clamped.
//! Failure to read or set a limit prevents process spawn.
//!
//! A non-empty configuration is rejected on non-Unix platforms when the runner is created.
//!
//! ## Limits
//!
//! | Field                 | Resource        |
//! |-----------------------|-----------------|
//! | `max_open_files`      | `RLIMIT_NOFILE` |
//! | `max_file_size_bytes` | `RLIMIT_FSIZE`  |
//! | `disable_core_dumps`  | `RLIMIT_CORE`   |
#[cfg(feature = "host-process")]
use std::process::Command;

#[cfg(all(feature = "host-process", not(unix)))]
use tracing::warn;

/// POSIX limits applied to a host process.
///
/// This type changes only soft limits.
/// Values above the inherited hard limit are clamped.
/// Non-empty settings require Unix.
#[derive(Debug, Clone, Default)]
pub struct RlimitConfig {
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`).
    ///
    /// `None` preserves the inherited soft limit.
    pub max_open_files: Option<u64>,
    /// Maximum size of created files in bytes (`RLIMIT_FSIZE`).
    ///
    /// `None` preserves the inherited soft limit.
    pub max_file_size_bytes: Option<u64>,
    /// Sets the core-file size limit to zero.
    ///
    /// `false` preserves the inherited soft limit.
    pub disable_core_dumps: bool,
}

impl RlimitConfig {
    /// Returns `true` when no limit is configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.max_open_files.is_none()
            && self.max_file_size_bytes.is_none()
            && !self.disable_core_dumps
    }
}

/// Attaches process limits to a command.
#[cfg(feature = "host-process")]
pub(crate) fn attach_rlimits(cmd: &mut Command, config: &RlimitConfig) {
    if config.is_empty() {
        return;
    }

    #[cfg(unix)]
    {
        unix_impl::attach_rlimits(cmd, config);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
        warn!(
            ?config,
            "rlimit-based process limits requested on a non-Unix OS; limits will be ignored"
        );
    }
}

#[cfg(all(feature = "host-process", unix))]
mod unix_impl {
    use super::RlimitConfig;
    use crate::host::log::{pre_exec_log, pre_exec_log_errno};

    use std::io;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    pub(super) fn attach_rlimits(cmd: &mut Command, config: &RlimitConfig) {
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

    /// Resource identifier accepted by `getrlimit` and `setrlimit`.
    ///
    /// Linux and Android use `__rlimit_resource_t`.
    /// Other Unix platforms use `c_int`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    type RlimitResource = libc::__rlimit_resource_t;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type RlimitResource = libc::c_int;

    const NOFILE: RlimitResource = libc::RLIMIT_NOFILE as RlimitResource;
    const FSIZE: RlimitResource = libc::RLIMIT_FSIZE as RlimitResource;
    const CORE: RlimitResource = libc::RLIMIT_CORE as RlimitResource;

    /// Sets one soft limit and preserves its hard limit.
    ///
    /// A finite hard limit clamps `value`.
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

    #[cfg(test)]
    pub(crate) fn reduced_nofile_soft_limit_for_test() -> u64 {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };

        // SAFETY: `current` is a valid stack-local rlimit struct.
        assert_eq!(unsafe { libc::getrlimit(NOFILE, &mut current) }, 0);

        if current.rlim_cur == libc::RLIM_INFINITY {
            return 64;
        }
        assert!(
            current.rlim_cur > 3,
            "RLIMIT_NOFILE is too low to run the test child"
        );
        current.rlim_cur - 1
    }
}

#[cfg(all(test, feature = "host-process", unix))]
pub(crate) use unix_impl::reduced_nofile_soft_limit_for_test;

#[cfg(all(test, feature = "host-process"))]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_noop() {
        let config = RlimitConfig::default();
        assert!(config.is_empty());

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
}
