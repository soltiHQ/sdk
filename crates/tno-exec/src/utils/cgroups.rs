//! cgroup v2 resource limits for subprocess-based runners (Linux-only).
//!
//! ## Overview
//!
//! This module exposes structured API for applying cgroup v2 limits to child processes created via `tokio::process::Command`.
//! - On **Linux with cgroup v2**, limits are applied by creating a dedicated cgroup under `/sys/fs/cgroup/<group_name>`
//! - On **non-Linux platforms**, limits are ignored: a warning is emitted and the call returns `Ok(())`.
use tokio::process::Command;

use crate::ExecError;

/// CPU limit (`cpu.max`) for cgroup v2.
#[derive(Debug, Clone, Copy)]
pub struct CpuMax {
    /// CPU quota in microseconds for each period.
    ///
    /// `None` -> unlimited.
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
    /// Max num of processes allowed (pids).
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
pub fn attach_cgroup_limits(
    cmd: &mut Command,
    name: &str,
    limits: &CgroupLimits,
) -> Result<(), ExecError> {
    if limits.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::attach(cmd, name, limits);
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            "cgroup v2 limits requested for group '{}', but OS={} does not support them; limits will be ignored",
            name,
            std::env::consts::OS
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{CgroupLimits, CpuMax};

    use std::{
        fs,
        io::{self, Write},
        path::{Path, PathBuf},
    };

    use tokio::process::Command;

    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};

    const CONTROLLERS_FILE: &str = "cgroup.controllers";
    const CGROUP_ROOT: &str = "/sys/fs/cgroup";

    pub fn attach(cmd: &mut Command, name: &str, limits: &CgroupLimits) {
        let name = name.to_string();
        let limits = limits.clone();

        unsafe {
            cmd.pre_exec(move || {
                if !is_cgroup_v2(Path::new(CGROUP_ROOT)) {
                    pre_exec_log(
                        b"tno-exec: cgroup v2 not detected at /sys/fs/cgroup (missing cgroup.controllers); limits will be ignored\n",
                    );
                    return Ok(());
                }

                let cg_dir = Path::new(CGROUP_ROOT).join(&name);

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

                if let Err(e) = add_self_to_cgroup(&cg_dir) {
                    pre_exec_log(b"tno-exec: failed to attach PID to cgroup; limits will be ignored\n");
                    if let Some(code) = e.raw_os_error() {
                        pre_exec_log_errno(code);
                    }
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
            Some(q) => format!("{} {}\n", q, limit.period),
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
        let r = attach_cgroup_limits(&mut cmd, "cg-empty", &limits);
        assert!(r.is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn attach_limited_group_does_not_error() {
        // We only validate that attach() itself is non-failing.
        // Actual cgroup mount presence is runtime-dependent.
        let limits = CgroupLimits {
            cpu: Some(CpuMax::default()),
            memory: Some(128 * 1024 * 1024),
            pids: Some(32),
        };

        let mut cmd = Command::new("true");
        let r = attach_cgroup_limits(&mut cmd, "tno-test-cg", &limits);
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
        let r = attach_cgroup_limits(&mut cmd, "cg-any", &limits);
        assert!(
            r.is_ok(),
            "non-Linux must ignore limits but still return Ok"
        );
    }
}
