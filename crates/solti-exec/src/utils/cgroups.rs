//! # Linux cgroup v2
//!
//! [`CgroupLimits`] controls CPU, memory, processes, and threads for one attempt.
//!
//! ## Flow
//!
//! ```text
//! runner construction
//!      └── resolve cgroup parent
//!                 ▼
//!            task attempt
//!      ├── create child cgroup
//!      ├── write configured limits
//!      ├── join before execve
//!      └── remove after process cleanup
//! ```
//!
//! An explicit parent must be an existing cgroup v2 directory.
//! Without one, the current process cgroup is used.

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::fs::File;

use tokio::process::Command;

use crate::ExecError;

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

/// Resource limits applied to one subprocess attempt.
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

/// Cgroup prepared before the subprocess is forked.
pub(crate) struct PreparedCgroup {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    procs: Option<File>,
    cleanup_on_drop: bool,
}

impl PreparedCgroup {
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
pub(crate) fn resolve_cgroup_parent(explicit: Option<&Path>) -> Result<PathBuf, ExecError> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::resolve_parent(explicit)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = explicit;
        Err(ExecError::InvalidRunnerConfig(format!(
            "cgroup v2 is not supported on {}",
            std::env::consts::OS
        )))
    }
}

/// Creates an attempt cgroup and applies its limits.
pub(crate) fn prepare_cgroup(
    parent: &Path,
    name: &str,
    limits: &CgroupLimits,
) -> Result<PreparedCgroup, ExecError> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::prepare(parent, name, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, name, limits);
        Err(ExecError::InvalidRunnerConfig(format!(
            "cgroup v2 is not supported on {}",
            std::env::consts::OS
        )))
    }
}

/// Attaches cgroup membership to a command.
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
pub(crate) fn cleanup_cgroup(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(path)
}

/// Builds a cgroup name for one task build.
pub(crate) fn build_cgroup_name(runner: &str, slot: &str, seq: u64, timestamp: u64) -> String {
    format!("{runner}-{slot}-{seq:x}-{timestamp:x}")
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::{Path, PathBuf};

    use tokio::process::Command;

    use super::{CgroupLimits, CpuMax, PreparedCgroup};
    use crate::ExecError;
    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};

    const CGROUP_ROOT: &str = "/sys/fs/cgroup";

    pub(super) fn resolve_parent(explicit: Option<&Path>) -> Result<PathBuf, ExecError> {
        let parent = match explicit {
            Some(path) => {
                if !path.is_absolute() {
                    return Err(ExecError::InvalidRunnerConfig(format!(
                        "cgroup parent must be absolute: {}",
                        path.display()
                    )));
                }
                path.to_path_buf()
            }
            None => current_process_cgroup()?,
        };

        let parent = parent.canonicalize().map_err(|e| {
            ExecError::InvalidRunnerConfig(format!(
                "cgroup parent {} cannot be resolved: {e}",
                parent.display()
            ))
        })?;
        if !parent.join("cgroup.procs").is_file() {
            return Err(ExecError::InvalidRunnerConfig(format!(
                "{} is not a cgroup v2 directory",
                parent.display()
            )));
        }
        Ok(parent)
    }

    fn current_process_cgroup() -> Result<PathBuf, ExecError> {
        let membership = fs::read_to_string("/proc/self/cgroup").map_err(ExecError::Io)?;
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
                ExecError::InvalidRunnerConfig(
                    "cannot find unified cgroup v2 membership in /proc/self/cgroup".into(),
                )
            })?;

        Ok(Path::new(CGROUP_ROOT).join(relative.trim_start_matches('/')))
    }

    pub(super) fn prepare(
        parent: &Path,
        name: &str,
        limits: &CgroupLimits,
    ) -> Result<PreparedCgroup, ExecError> {
        let path = parent.join(name);
        fs::create_dir(&path).map_err(ExecError::Io)?;

        let prepared = (|| -> io::Result<PreparedCgroup> {
            apply_limits(&path, limits)?;
            let procs = OpenOptions::new()
                .write(true)
                .open(path.join("cgroup.procs"))?;
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
                Err(ExecError::Io(io::Error::new(
                    source.kind(),
                    format!("failed to prepare cgroup {}: {source}", path.display()),
                )))
            }
        }
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
        let written = unsafe { libc::write(fd, pid.as_ptr().cast(), pid.len()) };
        if written == pid.len() as isize {
            return Ok(());
        }

        let error = if written < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(io::ErrorKind::WriteZero, "short write to cgroup.procs")
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
        use super::format_pid;

        #[test]
        fn formats_pid_for_cgroup_procs() {
            let mut buffer = [0u8; 24];
            assert_eq!(format_pid(42, &mut buffer), b"42\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_name_is_stable() {
        assert_eq!(
            build_cgroup_name("runner", "slot", 42, 1000),
            "runner-slot-2a-3e8"
        );
    }

    #[test]
    fn empty_limits_are_detected() {
        assert!(CgroupLimits::default().is_empty());
    }

    #[test]
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
