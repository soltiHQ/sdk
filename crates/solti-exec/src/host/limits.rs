//! # POSIX resource limits
//!
//! [`RlimitConfig`] sets process limit ceilings on Unix.
//!
//! ## Flow
//!
//! ```text
//! RlimitConfig
//!      │ backend preparation
//!      ▼
//! read inherited hard limit
//!      ▼
//! prepare clamped ceiling
//!      │ pre_exec
//!      ▼
//! set soft and hard limits
//!      ▼
//! execve with updated ceiling
//! ```
//!
//! A request above the inherited hard limit is clamped.
//! Both limits are set to the resulting value.
//! A Linux process retaining `CAP_SYS_RESOURCE` can raise the hard limit again.
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

#[cfg(feature = "host-process")]
use crate::host::HostProcessError;

/// POSIX limit ceilings applied to a host process.
///
/// Each value sets both the soft and hard limit.
/// Values above the inherited hard limit are clamped.
/// A Linux process retaining `CAP_SYS_RESOURCE` can raise the hard limit again.
/// Non-empty settings require Unix.
#[derive(Debug, Clone, Default)]
pub struct RlimitConfig {
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`).
    ///
    /// `None` preserves the inherited limits.
    pub max_open_files: Option<u64>,
    /// Maximum size of created files in bytes (`RLIMIT_FSIZE`).
    ///
    /// `None` preserves the inherited limits.
    pub max_file_size_bytes: Option<u64>,
    /// Sets both core-file size limits to zero.
    ///
    /// `false` preserves the inherited limits.
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

    #[cfg(feature = "host-process")]
    pub(crate) fn prepare(&self) -> Result<PreparedRlimits, HostProcessError> {
        #[cfg(unix)]
        return unix_impl::prepare(self);

        #[cfg(not(unix))]
        Ok(PreparedRlimits {})
    }
}

/// Process limit ceilings resolved before process creation.
///
/// Requests above inherited hard limits are clamped in these values.
/// The limits are applied when a prepared attempt starts its process.
#[cfg(feature = "host-process")]
#[derive(Debug, Clone, Default)]
pub struct PreparedRlimits {
    #[cfg(unix)]
    nofile: Option<unix_impl::PreparedLimit>,
    #[cfg(unix)]
    fsize: Option<unix_impl::PreparedLimit>,
    #[cfg(unix)]
    core: Option<unix_impl::PreparedLimit>,
}

#[cfg(feature = "host-process")]
impl PreparedRlimits {
    /// Returns the resolved `RLIMIT_NOFILE` ceiling.
    pub fn max_open_files(&self) -> Option<u64> {
        #[cfg(unix)]
        return self.nofile.map(unix_impl::PreparedLimit::ceiling);

        #[cfg(not(unix))]
        None
    }

    /// Returns the resolved `RLIMIT_FSIZE` ceiling in bytes.
    pub fn max_file_size_bytes(&self) -> Option<u64> {
        #[cfg(unix)]
        return self.fsize.map(unix_impl::PreparedLimit::ceiling);

        #[cfg(not(unix))]
        None
    }

    /// Returns the resolved `RLIMIT_CORE` ceiling in bytes.
    pub fn core_dump_size_bytes(&self) -> Option<u64> {
        #[cfg(unix)]
        return self.core.map(unix_impl::PreparedLimit::ceiling);

        #[cfg(not(unix))]
        None
    }

    fn is_empty(&self) -> bool {
        #[cfg(unix)]
        return self.nofile.is_none() && self.fsize.is_none() && self.core.is_none();

        #[cfg(not(unix))]
        true
    }
}

/// Attaches prepared process limits to a command.
#[cfg(feature = "host-process")]
pub(crate) fn attach_rlimits(cmd: &mut Command, prepared: &PreparedRlimits) {
    if prepared.is_empty() {
        return;
    }

    #[cfg(unix)]
    unix_impl::attach_rlimits(cmd, prepared);

    #[cfg(not(unix))]
    let _ = (cmd, prepared);
}

#[cfg(all(feature = "host-process", unix))]
mod unix_impl {
    use super::{PreparedRlimits, RlimitConfig};
    use crate::host::{
        HostProcessError,
        log::{pre_exec_log, pre_exec_log_errno},
    };

    use std::io;
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    #[cfg(target_os = "linux")]
    type LimitValue = u64;
    #[cfg(not(target_os = "linux"))]
    type LimitValue = libc::rlim_t;

    #[cfg(target_os = "linux")]
    pub(super) const LIMIT_INFINITY: LimitValue = u64::MAX;
    #[cfg(not(target_os = "linux"))]
    pub(super) const LIMIT_INFINITY: LimitValue = libc::RLIM_INFINITY;

    #[allow(clippy::absurd_extreme_comparisons)]
    fn is_infinite_or_beyond(value: LimitValue) -> bool {
        value >= LIMIT_INFINITY
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub(super) struct PreparedLimit {
        current: LimitValue,
        maximum: LimitValue,
    }

    impl PreparedLimit {
        pub(super) fn ceiling(self) -> u64 {
            #[cfg(target_os = "linux")]
            return self.maximum;

            #[cfg(not(target_os = "linux"))]
            {
                #[allow(clippy::useless_conversion)]
                return u64::try_from(self.maximum)
                    .expect("native rlimit value must be representable by u64");
            }
        }
    }

    #[cfg(target_os = "linux")]
    const _: [(); 16] = [(); std::mem::size_of::<PreparedLimit>()];

    pub(super) fn prepare(config: &RlimitConfig) -> Result<PreparedRlimits, HostProcessError> {
        let nofile = config
            .max_open_files
            .map(|value| prepare_limit("rlimits.max_open_files", NOFILE, value))
            .transpose()?;
        let fsize = config
            .max_file_size_bytes
            .map(|value| prepare_limit("rlimits.max_file_size_bytes", FSIZE, value))
            .transpose()?;
        let core = config
            .disable_core_dumps
            .then(|| prepare_limit("rlimits.disable_core_dumps", CORE, 0))
            .transpose()?;

        Ok(PreparedRlimits {
            nofile,
            fsize,
            core,
        })
    }

    pub(super) fn attach_rlimits(cmd: &mut Command, prepared: &PreparedRlimits) {
        let nofile = prepared.nofile;
        let fsize = prepared.fsize;
        let core = prepared.core;

        // SAFETY:
        // The pre_exec closure runs between fork() and execve() in the child process.
        // Limits are fully prepared in the parent.
        // The child only calls the limit-setting syscall and raw logging helpers.
        // Error paths use io::Error::last_os_error() which stores errno inline without heap allocation (Rust >= 1.74).
        unsafe {
            cmd.pre_exec(move || {
                if let Some(limit) = nofile
                    && let Err(error) = apply_rlimit(NOFILE, &limit)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_NOFILE: ");
                    if let Some(code) = error.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(error);
                }
                if let Some(limit) = fsize
                    && let Err(error) = apply_rlimit(FSIZE, &limit)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_FSIZE: ");
                    if let Some(code) = error.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(error);
                }
                if let Some(limit) = core
                    && let Err(error) = apply_rlimit(CORE, &limit)
                {
                    pre_exec_log(b"solti-exec: failed to set RLIMIT_CORE: ");
                    if let Some(code) = error.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Err(error);
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

    fn prepare_limit(
        name: &str,
        resource: RlimitResource,
        value: u64,
    ) -> Result<PreparedLimit, HostProcessError> {
        let requested = LimitValue::try_from(value).ok();
        if requested.is_none_or(is_infinite_or_beyond) {
            return Err(HostProcessError::InvalidConfig(format!(
                "{name} must be a finite value representable by rlim_t"
            )));
        }
        let requested = requested.expect("validated rlimit must be present");
        let inherited = read_limit(resource).map_err(HostProcessError::Io)?;
        let ceiling = if inherited.maximum == LIMIT_INFINITY {
            requested
        } else {
            requested.min(inherited.maximum)
        };
        Ok(PreparedLimit {
            current: ceiling,
            maximum: ceiling,
        })
    }

    #[cfg(target_os = "linux")]
    fn read_limit(resource: RlimitResource) -> io::Result<PreparedLimit> {
        let mut current = PreparedLimit {
            current: 0,
            maximum: 0,
        };
        // SAFETY: `current` is the kernel rlimit64 representation on Linux.
        let result = unsafe {
            libc::syscall(
                libc::SYS_prlimit64,
                0,
                resource,
                std::ptr::null::<PreparedLimit>(),
                &mut current,
            )
        };
        if result != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(current)
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn read_limit(resource: RlimitResource) -> io::Result<PreparedLimit> {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `current` is a valid stack-local rlimit struct.
        if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(PreparedLimit {
                current: current.rlim_cur,
                maximum: current.rlim_max,
            })
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_rlimit(resource: RlimitResource, limit: &PreparedLimit) -> io::Result<()> {
        // SAFETY: `limit` is the immutable kernel rlimit64 representation prepared in the parent.
        let result = unsafe {
            libc::syscall(
                libc::SYS_prlimit64,
                0,
                resource,
                limit,
                std::ptr::null_mut::<PreparedLimit>(),
            )
        };
        if result != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn apply_rlimit(resource: RlimitResource, limit: &PreparedLimit) -> io::Result<()> {
        let native = libc::rlimit {
            rlim_cur: limit.current,
            rlim_max: limit.maximum,
        };
        // SAFETY: `native` is immutable stack storage with prepared values.
        if unsafe { libc::setrlimit(resource, &native) } != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(test)]
    pub(crate) fn reduced_nofile_limit_for_test() -> u64 {
        let current = read_limit(NOFILE).unwrap();

        if current.current == LIMIT_INFINITY {
            return 64;
        }
        assert!(
            current.current > 3,
            "RLIMIT_NOFILE is too low to run the test child"
        );
        current.current - 1
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn requested_ceiling_is_clamped_to_inherited_hard_limit() {
            let inherited = read_limit(NOFILE).unwrap().maximum;
            let Some(requested) = inherited.checked_add(1) else {
                return;
            };
            if is_infinite_or_beyond(requested) {
                return;
            }

            let config = RlimitConfig {
                max_open_files: Some(requested),
                ..Default::default()
            };
            let prepared = prepare(&config).unwrap();
            assert_eq!(prepared.max_open_files(), Some(inherited));
        }
    }
}

#[cfg(all(test, feature = "host-process", unix))]
pub(crate) use unix_impl::reduced_nofile_limit_for_test;

#[cfg(all(test, feature = "host-process"))]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_noop() {
        let config = RlimitConfig::default();
        assert!(config.is_empty());

        let mut cmd = Command::new("sh");
        let prepared = config.prepare().unwrap();
        attach_rlimits(&mut cmd, &prepared);
    }

    #[cfg(unix)]
    #[test]
    fn infinite_limit_is_rejected() {
        #[allow(clippy::unnecessary_cast)]
        let infinity: u64 = unix_impl::LIMIT_INFINITY as u64;
        let config = RlimitConfig {
            max_open_files: Some(infinity),
            ..Default::default()
        };

        let error = config.prepare().unwrap_err().to_string();
        assert!(error.contains("finite value"), "got: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn configured_limit_is_a_hard_ceiling() {
        let requested = reduced_nofile_limit_for_test();
        let config = RlimitConfig {
            max_open_files: Some(requested),
            ..Default::default()
        };
        let mut command = Command::new("sh");
        command.arg("-c").arg("ulimit -Sn; ulimit -Hn");
        let prepared = config.prepare().unwrap();
        attach_rlimits(&mut command, &prepared);

        let output = command.output().unwrap();
        assert!(output.status.success());
        let limits = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|value| value.parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(limits, [requested, requested]);
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
        let prepared = config.prepare().unwrap();
        attach_rlimits(&mut cmd, &prepared);
    }
}
