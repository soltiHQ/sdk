//! cgroup v2 resource limits for subprocess-based runners.
//!
//! ## Overview
//!
//! This module exposes structured API for applying cgroup v2 limits to child processes created via `tokio::process::Command`.
//! - On **Linux with cgroup v2**, limits are applied by creating a cgroup and placing the child PID via `pre_exec` hook.
//! - On **non-Linux platforms**, limits are ignored: a warning is emitted and the call returns `Ok(())`.
use tokio::process::Command;

use crate::ExecError;

/// CPU limit (`cpu.max`) for cgroup v2.
/// - `<quota> <period>` sets a quota/period time window.
#[derive(Debug, Clone, Copy)]
pub struct CpuMax {
    /// CPU quota in microseconds for each period. (`None` is unlimited).
    pub quota: Option<u64>,
    /// Period in microseconds (usually 100_000 = 100ms).
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

/// Declarative cgroup limits for a child process.
///
/// All fields are optional. `None` means "no limit".
#[derive(Debug, Clone, Default)]
pub struct CgroupLimits {
    /// CPU limit.
    pub cpu: Option<CpuMax>,
    /// Memory limit in bytes.
    pub memory: Option<u64>,
    /// Max number of processes (pids).
    pub pids: Option<u64>,
    /// If `true`, cgroup setup failures abort the subprocess spawn.
    /// If `false` (default), failures are logged and the process runs without cgroup isolation.
    pub fail_on_error: bool,
}

impl CgroupLimits {
    /// Returns `true` if all limits are `None`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.pids.is_none()
    }
}

/// Prepare cgroup v2 limits: create cgroup directory and write limit files.
///
/// This phase runs **before** subprocess spawn (in normal async context),
/// so it can safely use `std::fs` operations.
///
/// Returns `Ok(true)` if the cgroup was created and limits applied (the command needs
/// a `pre_exec` hook to join the cgroup). Returns `Ok(false)` if cgroups are unavailable
/// or the platform is not Linux.
///
/// # Cgroup lifecycle
/// - Kernel auto-removes empty cgroups when all processes exit
/// - Use [`cleanup_cgroup`] to explicitly remove a cgroup (best-effort)
pub(crate) fn prepare_cgroup(cgroup_name: &str, limits: &CgroupLimits) -> Result<bool, ExecError> {
    if limits.is_empty() {
        return Ok(false);
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::prepare(cgroup_name, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            "cgroup v2 limits requested for '{}', but OS={} does not support them; limits will be ignored",
            cgroup_name,
            std::env::consts::OS
        );
        Ok(false)
    }
}

/// Attach cgroup v2 join hook to a `tokio::process::Command`.
///
/// The `pre_exec` hook only writes the child PID to `cgroup.procs` using raw
/// libc syscalls — fully async-signal-safe.
///
/// Must be called after [`prepare_cgroup`] succeeds with `Ok(true)`.
pub(crate) fn attach_cgroup(
    cmd: &mut Command,
    cgroup_name: &str,
    limits: &CgroupLimits,
) -> Result<(), ExecError> {
    if limits.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::attach_join_hook(cmd, cgroup_name, limits.fail_on_error);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (&cmd, cgroup_name, limits);
    }
    Ok(())
}

/// Attempt to remove a cgroup directory.
/// Best-effort cgroup cleanup.
///
/// Cgroup removal can fail for many reasons (permission denied, busy, not found,
/// read-only cgroupfs in containers, etc.). All failures are logged and swallowed
/// because the kernel auto-removes empty cgroups when all member processes exit.
#[cfg(target_os = "linux")]
pub fn cleanup_cgroup(cgroup_name: &str) -> Result<(), ExecError> {
    use std::path::Path;

    let full_path = Path::new("/sys/fs/cgroup").join(cgroup_name);

    match std::fs::remove_dir(&full_path) {
        Ok(()) => {
            tracing::debug!("removed cgroup: {}", cgroup_name);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::trace!("cgroup '{}' not found (already removed)", cgroup_name);
        }
        Err(e) => {
            tracing::debug!(
                "cgroup '{}' cleanup skipped: {} (errno={:?})",
                cgroup_name,
                e,
                e.raw_os_error(),
            );
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn cleanup_cgroup(_cgroup_name: &str) -> Result<(), ExecError> {
    Ok(())
}

/// Build a unique cgroup name from components.
///
/// Format: `{runner_tag}-{slot}-{seq:x}-{timestamp:x}`
pub fn build_cgroup_name(runner_tag: &str, slot: &str, seq: u64, timestamp: u64) -> String {
    format!("{}-{}-{:x}-{:x}", runner_tag, slot, seq, timestamp)
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{CgroupLimits, CpuMax};
    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};

    use std::{
        fs, io,
        path::{Path, PathBuf},
    };

    use tokio::process::Command;

    const CONTROLLERS_FILE: &str = "cgroup.controllers";
    const CGROUP_ROOT: &str = "/sys/fs/cgroup";
    const CGROUP_PROCS_SUFFIX: &str = "/cgroup.procs";

    /// Phase 1: Create cgroup directory and write limit files.
    /// Runs before spawn in normal async context — safe to use std::fs.
    pub fn prepare(cgroup_name: &str, limits: &CgroupLimits) -> Result<bool, crate::ExecError> {
        if !is_cgroup_v2(Path::new(CGROUP_ROOT)) {
            tracing::warn!("cgroup v2 not detected at /sys/fs/cgroup; limits will be ignored");
            return if limits.fail_on_error {
                Err(crate::ExecError::InvalidRunnerConfig(
                    "cgroup v2 not available".into(),
                ))
            } else {
                Ok(false)
            };
        }

        let cg_dir = Path::new(CGROUP_ROOT).join(cgroup_name);
        fs::create_dir_all(&cg_dir).map_err(|e| {
            crate::ExecError::Io(io::Error::other(format!(
                "failed to create cgroup directory '{}': {e}",
                cg_dir.display()
            )))
        })?;
        apply_limits(&cg_dir, limits).map_err(|e| {
            crate::ExecError::Io(io::Error::other(format!(
                "failed to apply cgroup limits for '{}': {e}",
                cg_dir.display()
            )))
        })?;
        Ok(true)
    }

    /// Phase 2: pre_exec hook that only writes the child PID to cgroup.procs.
    /// Uses raw libc syscalls — fully async-signal-safe.
    pub fn attach_join_hook(cmd: &mut Command, cgroup_name: &str, fail_on_error: bool) {
        let mut procs_path = Vec::with_capacity(
            CGROUP_ROOT.len() + 1 + cgroup_name.len() + CGROUP_PROCS_SUFFIX.len(),
        );
        procs_path.extend_from_slice(CGROUP_ROOT.as_bytes());
        procs_path.push(b'/');
        procs_path.extend_from_slice(cgroup_name.as_bytes());
        procs_path.extend_from_slice(CGROUP_PROCS_SUFFIX.as_bytes());
        procs_path.push(0); // null terminator for C

        // SAFETY: The pre_exec closure runs between fork() and execve().
        // It uses only libc::open, libc::write, libc::close, libc::getpid —
        // all async-signal-safe per POSIX. No heap allocation occurs.
        unsafe {
            cmd.pre_exec(move || join_cgroup_raw(&procs_path, fail_on_error));
        }
    }

    fn is_cgroup_v2(root: &Path) -> bool {
        root.join(CONTROLLERS_FILE).is_file()
    }

    fn apply_limits(dir: &Path, limits: &CgroupLimits) -> io::Result<()> {
        if let Some(cpu) = limits.cpu {
            write_cpu_max(dir.join("cpu.max"), cpu)?;
        }
        if let Some(mem) = limits.memory {
            write_limit(dir.join("memory.max"), mem)?;
        }
        if let Some(pids) = limits.pids {
            write_limit(dir.join("pids.max"), pids)?;
        }
        Ok(())
    }

    fn write_cpu_max(path: PathBuf, limit: CpuMax) -> io::Result<()> {
        let content = match limit.quota {
            None => format!("max {}\n", limit.period),
            Some(q) => format!("{q} {}\n", limit.period),
        };
        fs::write(path, content)
    }

    fn write_limit(path: PathBuf, val: u64) -> io::Result<()> {
        fs::write(path, format!("{val}\n"))
    }

    /// Write PID to cgroup.procs using raw libc syscalls only.
    /// Fully async-signal-safe — no heap allocation, no mutexes.
    fn join_cgroup_raw(procs_path_cstr: &[u8], fail_on_error: bool) -> io::Result<()> {
        // SAFETY: procs_path_cstr is a null-terminated byte string built before fork().
        // libc::open is async-signal-safe per POSIX.
        let fd = unsafe {
            libc::open(
                procs_path_cstr.as_ptr() as *const libc::c_char,
                libc::O_WRONLY,
            )
        };
        if fd < 0 {
            pre_exec_log(b"solti-exec: failed to open cgroup.procs\n");
            let errno = unsafe { *libc::__errno_location() };
            pre_exec_log_errno(errno);
            return if fail_on_error {
                Err(io::Error::from_raw_os_error(errno))
            } else {
                Ok(())
            };
        }

        // Format PID into a stack buffer (no allocation).
        // SAFETY: getpid() is async-signal-safe, always succeeds.
        let pid = unsafe { libc::getpid() };
        let mut buf = [0u8; 24];
        let pid_str = format_pid(pid, &mut buf);

        // SAFETY: fd is a valid open file descriptor. pid_str is a valid byte slice.
        // libc::write is async-signal-safe per POSIX.
        let written =
            unsafe { libc::write(fd, pid_str.as_ptr() as *const libc::c_void, pid_str.len()) };
        // SAFETY: libc::close is async-signal-safe.
        unsafe { libc::close(fd) };

        if written < 0 {
            pre_exec_log(b"solti-exec: failed to write PID to cgroup.procs\n");
            let errno = unsafe { *libc::__errno_location() };
            pre_exec_log_errno(errno);
            return if fail_on_error {
                Err(io::Error::from_raw_os_error(errno))
            } else {
                Ok(())
            };
        }

        Ok(())
    }

    /// Format a PID into a stack buffer as `"<pid>\n"`. Returns the written slice.
    fn format_pid(pid: libc::pid_t, buf: &mut [u8; 24]) -> &[u8] {
        let mut n = pid as u32;
        let mut idx = buf.len() - 1;
        buf[idx] = b'\n';
        if n == 0 {
            idx -= 1;
            buf[idx] = b'0';
        } else {
            while n > 0 {
                idx -= 1;
                buf[idx] = b'0' + (n % 10) as u8;
                n /= 10;
            }
        }
        &buf[idx..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_limits_are_noop() {
        let limits = CgroupLimits::default();
        assert!(limits.is_empty());

        let mut cmd = Command::new("sh");
        let r = attach_cgroup(&mut cmd, "test-cgroup", &limits);
        assert!(r.is_ok());
    }

    #[test]
    fn build_cgroup_name_simple_case() {
        let name = build_cgroup_name("runner", "slot", 42, 1000);
        let parts: Vec<&str> = name.split('-').collect();

        assert_eq!(name, "runner-slot-2a-3e8");
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "runner");
        assert_eq!(parts[1], "slot");
        assert_eq!(u64::from_str_radix(parts[2], 16).unwrap(), 42);
        assert_eq!(u64::from_str_radix(parts[3], 16).unwrap(), 1000);
    }

    #[test]
    fn build_cgroup_name_with_dashes() {
        let name = build_cgroup_name("prod-runner", "demo-task", 42, 1733045913);
        let timestamp_hex = format!("{:x}", 1733045913u64);

        assert!(name.starts_with("prod-runner-"));
        assert!(name.contains("-demo-task-"));
        assert!(name.contains("-2a-"));
        assert!(name.ends_with(&format!("-{}", timestamp_hex)));
    }

    #[test]
    fn build_cgroup_name_hex_values() {
        let name = build_cgroup_name("r", "s", 0, 0);
        assert_eq!(name, "r-s-0-0");
        let name = build_cgroup_name("r", "s", 255, 255);
        assert_eq!(name, "r-s-ff-ff");
        let name = build_cgroup_name("r", "s", 4096, 65536);
        assert_eq!(name, "r-s-1000-10000");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn attach_with_limits_does_not_error() {
        let limits = CgroupLimits {
            cpu: Some(CpuMax::default()),
            memory: Some(128 * 1024 * 1024),
            pids: Some(32),
            ..Default::default()
        };
        let name = build_cgroup_name("test", "slot", 1, 1733045913);
        let mut cmd = Command::new("true");
        let r = attach_cgroup(&mut cmd, &name, &limits);
        assert!(r.is_ok());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_platforms_ignore_limits() {
        let limits = CgroupLimits {
            cpu: Some(CpuMax::default()),
            memory: Some(1),
            pids: Some(1),
            ..Default::default()
        };
        let mut cmd = Command::new("true");
        let r = attach_cgroup(&mut cmd, "test-cgroup", &limits);
        assert!(
            r.is_ok(),
            "non-Linux must ignore limits but still return Ok"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_nonexistent_cgroup_succeeds() {
        let name = build_cgroup_name("test", "nonexistent", 999, 1733045913);
        let r = cleanup_cgroup(&name);
        assert!(r.is_ok(), "cleanup of nonexistent cgroup should succeed");
    }
}
