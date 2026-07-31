//! # Linux cgroup v2
//!
//! [`CgroupLimits`] controls CPU, memory, processes, and threads for one attempt.
//!
//! ## Flow
//!
//! ```text
//! backend construction
//!      └── resolve cgroup parent
//!                 ▼
//!          execution attempt
//!      ├── create child cgroup
//!      ├── write configured limits
//!      ├── join before execve
//!      └── remove after process cleanup
//! ```
//!
//! An explicit parent must be an existing cgroup v2 directory.
//! Without one, the current process cgroup is used.

#[cfg(feature = "host-process")]
use std::path::{Path, PathBuf};

#[cfg(all(feature = "host-process", target_os = "linux"))]
use std::fs::File;

#[cfg(feature = "host-process")]
use std::process::Command;

#[cfg(feature = "host-process")]
use super::HostProcessError;

/// CPU bandwidth limit written to `cpu.max`.
///
/// The default represents `max 100000`.
/// A zero period or zero quota is invalid.
#[derive(Debug, Clone, Copy)]
pub struct CpuMax {
    /// CPU time allowed per period, in microseconds.
    ///
    /// `None` means no quota.
    pub quota: Option<u64>,
    /// Accounting period in microseconds.
    pub period: u64,
}

impl Default for CpuMax {
    fn default() -> Self {
        Self {
            quota: None,
            period: 100_000,
        }
    }
}

/// Resource limits applied to one host process scope.
///
/// This type requires Linux cgroup v2.
/// At least one field must be set.
/// CPU periods and explicit quotas must be greater than zero.
/// Memory and process limits must be greater than zero.
#[derive(Debug, Clone, Default)]
pub struct CgroupLimits {
    /// CPU bandwidth limit.
    pub cpu: Option<CpuMax>,
    /// Maximum memory in bytes.
    pub memory: Option<u64>,
    /// Maximum number of processes and threads in the cgroup.
    pub pids: Option<u64>,
}

impl CgroupLimits {
    /// Returns `true` when no limit is configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.pids.is_none()
    }
}

/// Cgroup prepared before the host process is forked.
#[cfg(feature = "host-process")]
pub(crate) struct PreparedCgroup {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    procs: Option<File>,
    cleanup_on_drop: bool,
}

#[cfg(feature = "host-process")]
impl PreparedCgroup {
    /// Returns the cgroup directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(target_os = "linux")]
    fn into_procs(mut self) -> File {
        self.cleanup_on_drop = false;
        self.procs
            .take()
            .expect("prepared cgroup must own cgroup.procs")
    }
}

#[cfg(feature = "host-process")]
impl Drop for PreparedCgroup {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Err(error) = std::fs::remove_dir(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                cgroup = %self.path.display(),
                error = %error,
                "failed to clean up unused prepared cgroup",
            );
        }
    }
}

/// Resolves an explicit parent or the current process cgroup.
#[cfg(feature = "host-process")]
pub(crate) fn resolve_cgroup_parent(explicit: Option<&Path>) -> Result<PathBuf, HostProcessError> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::resolve_parent(explicit)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = explicit;
        Err(HostProcessError::InvalidConfig(format!(
            "cgroup v2 is not supported on {}",
            std::env::consts::OS
        )))
    }
}

/// Creates an attempt cgroup and applies its limits.
#[cfg(feature = "host-process")]
pub(crate) fn prepare_cgroup(
    parent: &Path,
    name: &str,
    limits: &CgroupLimits,
) -> Result<PreparedCgroup, HostProcessError> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::prepare(parent, name, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, name, limits);
        Err(HostProcessError::InvalidConfig(format!(
            "cgroup v2 is not supported on {}",
            std::env::consts::OS
        )))
    }
}

/// Attaches cgroup membership to a command.
#[cfg(feature = "host-process")]
pub(crate) fn attach_cgroup(cmd: &mut Command, prepared: PreparedCgroup) {
    #[cfg(target_os = "linux")]
    {
        linux_impl::attach(cmd, prepared);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cmd, prepared);
    }
}

/// Removes an empty attempt cgroup.
#[cfg(feature = "host-process")]
pub(crate) fn cleanup_cgroup(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(path)
}

#[cfg(all(feature = "host-process", target_os = "linux"))]
mod linux_impl {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd as _};
    use std::os::unix::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::super::HostProcessError;
    use super::{CgroupLimits, CpuMax, PreparedCgroup};
    use crate::host::log::{pre_exec_log, pre_exec_log_errno};

    const CGROUP_ROOT: &str = "/sys/fs/cgroup";

    pub(super) fn resolve_parent(explicit: Option<&Path>) -> Result<PathBuf, HostProcessError> {
        let parent = match explicit {
            Some(path) => {
                if !path.is_absolute() {
                    return Err(HostProcessError::InvalidConfig(format!(
                        "cgroup parent must be absolute: {}",
                        path.display()
                    )));
                }
                path.to_path_buf()
            }
            None => current_process_cgroup()?,
        };

        let parent = parent.canonicalize().map_err(|e| {
            HostProcessError::InvalidConfig(format!(
                "cgroup parent {} cannot be resolved: {e}",
                parent.display()
            ))
        })?;
        if !parent.join("cgroup.procs").is_file() {
            return Err(HostProcessError::InvalidConfig(format!(
                "{} is not a cgroup v2 directory",
                parent.display()
            )));
        }
        Ok(parent)
    }

    fn current_process_cgroup() -> Result<PathBuf, HostProcessError> {
        let membership = fs::read_to_string("/proc/self/cgroup").map_err(HostProcessError::Io)?;
        let relative = membership
            .lines()
            .find_map(|line| {
                let mut parts = line.splitn(3, ':');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("0"), Some(""), Some(path)) => Some(path),
                    _ => None,
                }
            })
            .ok_or_else(|| {
                HostProcessError::InvalidConfig(
                    "cannot find unified cgroup v2 membership in /proc/self/cgroup".into(),
                )
            })?;

        Ok(Path::new(CGROUP_ROOT).join(relative.trim_start_matches('/')))
    }

    pub(super) fn prepare(
        parent: &Path,
        name: &str,
        limits: &CgroupLimits,
    ) -> Result<PreparedCgroup, HostProcessError> {
        let path = parent.join(name);
        fs::create_dir(&path).map_err(HostProcessError::Io)?;

        let prepared = (|| -> io::Result<PreparedCgroup> {
            apply_limits(&path, limits)?;
            let procs = OpenOptions::new()
                .write(true)
                .open(path.join("cgroup.procs"))?;
            let procs = duplicate_child_fd(procs)?;
            Ok(PreparedCgroup {
                path: path.clone(),
                procs: Some(procs),
                cleanup_on_drop: true,
            })
        })();

        match prepared {
            Ok(prepared) => Ok(prepared),
            Err(source) => {
                if let Err(cleanup) = fs::remove_dir(&path) {
                    tracing::warn!(
                        cgroup = %path.display(),
                        error = %cleanup,
                        "failed to roll back cgroup setup",
                    );
                }
                Err(HostProcessError::Io(io::Error::new(
                    source.kind(),
                    format!("failed to prepare cgroup {}: {source}", path.display()),
                )))
            }
        }
    }

    /// Moves an internal child descriptor outside the standard descriptor range.
    ///
    /// The standard library replaces descriptors `0..=2` while preparing child
    /// stdio. A cgroup descriptor in that range would otherwise refer to a
    /// different file by the time the `pre_exec` hook runs.
    fn duplicate_child_fd(file: File) -> io::Result<File> {
        // SAFETY: `file` owns a valid descriptor. `F_DUPFD_CLOEXEC` creates a
        // second descriptor whose value is at least `3`.
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a newly owned descriptor returned by `fcntl`.
        Ok(unsafe { File::from_raw_fd(raw) })
    }

    fn apply_limits(path: &Path, limits: &CgroupLimits) -> io::Result<()> {
        if let Some(cpu) = limits.cpu {
            write_cpu_max(path.join("cpu.max"), cpu)?;
        }
        if let Some(memory) = limits.memory {
            fs::write(path.join("memory.max"), format!("{memory}\n"))?;
        }
        if let Some(pids) = limits.pids {
            fs::write(path.join("pids.max"), format!("{pids}\n"))?;
        }
        Ok(())
    }

    fn write_cpu_max(path: PathBuf, limit: CpuMax) -> io::Result<()> {
        let value = match limit.quota {
            Some(quota) => format!("{quota} {}\n", limit.period),
            None => format!("max {}\n", limit.period),
        };
        fs::write(path, value)
    }

    pub(super) fn attach(cmd: &mut Command, prepared: PreparedCgroup) {
        let procs = prepared.into_procs();

        // SAFETY: the hook uses only getpid and write. The open file is owned by
        // the closure in the parent and remains valid in the child until exec.
        unsafe {
            cmd.pre_exec(move || join(procs.as_raw_fd()));
        }
    }

    fn join(fd: libc::c_int) -> io::Result<()> {
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() };
        let mut buffer = [0u8; 24];
        let pid = format_pid(pid, &mut buffer);

        // SAFETY: fd refers to cgroup.procs and pid is a valid byte slice.
        let written = loop {
            let written = unsafe { libc::write(fd, pid.as_ptr().cast(), pid.len()) };
            if written >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break written;
            }
        };
        if written == pid.len() as isize {
            return Ok(());
        }

        let error = if written < 0 {
            io::Error::last_os_error()
        } else {
            // A cgroup membership write must consume the complete PID. Keep
            // the post-fork error path allocation-free.
            io::Error::from_raw_os_error(libc::EIO)
        };
        pre_exec_log(b"solti-exec: failed to join cgroup: ");
        if let Some(code) = error.raw_os_error() {
            pre_exec_log_errno(code);
        }
        Err(error)
    }

    fn format_pid(pid: i32, buffer: &mut [u8; 24]) -> &[u8] {
        let mut value = pid as u32;
        let mut index = buffer.len() - 1;
        buffer[index] = b'\n';
        if value == 0 {
            index -= 1;
            buffer[index] = b'0';
        } else {
            while value > 0 {
                index -= 1;
                buffer[index] = b'0' + (value % 10) as u8;
                value /= 10;
            }
        }
        &buffer[index..]
    }

    #[cfg(test)]
    mod tests {
        use std::os::fd::AsRawFd as _;

        use super::{duplicate_child_fd, format_pid};

        #[test]
        fn formats_pid_for_cgroup_procs() {
            let mut buffer = [0u8; 24];
            assert_eq!(format_pid(42, &mut buffer), b"42\n");
        }

        #[test]
        fn child_descriptor_is_above_the_standard_range() {
            let file = tempfile::tempfile().unwrap();
            let duplicated = duplicate_child_fd(file).unwrap();

            assert!(duplicated.as_raw_fd() >= 3);
            let flags = unsafe { libc::fcntl(duplicated.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_limits_are_detected() {
        assert!(CgroupLimits::default().is_empty());
    }

    #[test]
    #[cfg(feature = "host-process")]
    fn unused_prepared_cgroup_is_removed_on_drop() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let prepared = PreparedCgroup {
            path: path.clone(),
            #[cfg(target_os = "linux")]
            procs: None,
            cleanup_on_drop: true,
        };

        drop(prepared);
        assert!(!path.exists());
    }
}
