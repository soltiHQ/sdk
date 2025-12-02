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
}

impl CgroupLimits {
    /// Returns `true` if all limits are `None`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.pids.is_none()
    }
}

/// Attach cgroup v2 limits to a `tokio::process::Command`.
///
/// Creates a cgroup at `/sys/fs/cgroup/{cgroup_name}/` and places the child process into it.
///
/// # Cgroup lifecycle
/// - Kernel auto-removes empty cgroups when all processes exit
/// - Use [`cleanup_cgroup`] to explicitly remove a cgroup (best-effort)
///
/// # Arguments
/// - `cmd`: Command to attach cgroup to
/// - `cgroup_name`: Unique cgroup name (`{runner}-{slot}-{seq}-{timestamp}`)
/// - `limits`: Resource limits to apply
pub fn attach_cgroup(
    cmd: &mut Command,
    cgroup_name: &str,
    limits: &CgroupLimits,
) -> Result<(), ExecError> {
    if limits.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::attach(cmd, cgroup_name, limits);
    }
    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            "cgroup v2 limits requested for '{}', but OS={} does not support them; limits will be ignored",
            cgroup_name,
            std::env::consts::OS
        );
    }
    Ok(())
}

/// Attempt to remove a cgroup directory.
#[cfg(target_os = "linux")]
pub fn cleanup_cgroup(cgroup_name: &str) -> Result<(), ExecError> {
    use std::path::Path;

    let full_path = Path::new("/sys/fs/cgroup").join(cgroup_name);

    match std::fs::remove_dir(&full_path) {
        Ok(()) => {
            tracing::debug!("removed cgroup: {}", cgroup_name);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::trace!("cgroup '{}' not found (already removed)", cgroup_name);
            Ok(())
        }
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
            tracing::debug!("cgroup '{}' is busy, skipping cleanup", cgroup_name);
            Ok(())
        }
        Err(e) if e.raw_os_error() == Some(libc::EACCES) => {
            tracing::debug!("cgroup '{}' cleanup: permission denied", cgroup_name);
            Ok(())
        }
        Err(e) => {
            tracing::warn!("failed to remove cgroup '{}': {}", cgroup_name, e);
            Ok(())
        }
    }
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
        fs,
        io::{self, Write},
        path::{Path, PathBuf},
    };

    use tokio::process::Command;

    const CONTROLLERS_FILE: &str = "cgroup.controllers";
    const CGROUP_ROOT: &str = "/sys/fs/cgroup";

    pub fn attach(cmd: &mut Command, cgroup_name: &str, limits: &CgroupLimits) {
        let cgroup_name = cgroup_name.to_string();
        let limits = limits.clone();

        unsafe {
            cmd.pre_exec(move || {
                if !is_cgroup_v2(Path::new(CGROUP_ROOT)) {
                    pre_exec_log(
                        b"tno-exec: cgroup v2 not detected at /sys/fs/cgroup; limits will be ignored\n",
                    );
                    return Ok(());
                }

                let cg_dir = Path::new(CGROUP_ROOT).join(&cgroup_name);
                if let Err(e) = fs::create_dir_all(&cg_dir) {
                    pre_exec_log(b"tno-exec: failed to create cgroup directory; limits will be ignored\n");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Ok(());
                }
                if let Err(e) = apply_limits(&cg_dir, &limits) {
                    pre_exec_log(b"tno-exec: failed to apply cgroup limits; limits will be ignored\n");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
                    return Ok(());
                }
                // CRITICAL: This may fail with `EINVAL` for very short-lived processesthat complete before pre_exec finishes (~1-5ms window).
                //
                // Common errno values:
                // - EINVAL (22): Process state changed (e.g., already exec'd or exited)
                // - EACCES (13): Permission denied (should have been caught at mkdir)
                // - ESRCH  ( 3): Process doesn't exist (already terminated)
                if let Err(_e) = add_self_to_cgroup(&cg_dir) {
                    pre_exec_log(b"tno-exec: failed to attach PID to cgroup; limits will be ignored\n");
                    return Ok(());
                }
                Ok(())
            });
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

    fn add_self_to_cgroup(dir: &Path) -> io::Result<()> {
        let procs = dir.join("cgroup.procs");
        let mut f = fs::OpenOptions::new().write(true).open(&procs)?;
        let pid = unsafe { libc::getpid() };
        writeln!(f, "{pid}")?;
        Ok(())
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
