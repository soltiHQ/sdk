//! # Process resource limits

/// POSIX limit ceilings applied to a process.
///
/// Each value sets both the soft and hard limit.
#[derive(Debug, Clone, Default)]
pub struct RlimitConfig {
    /// Maximum number of open file descriptors (`RLIMIT_NOFILE`).
    ///
    /// `None` preserves the existing limit.
    pub max_open_files: Option<u64>,
    /// Maximum size of created files in bytes (`RLIMIT_FSIZE`).
    ///
    /// `None` preserves the existing limit.
    pub max_file_size_bytes: Option<u64>,
    /// Sets both core-file size limits to zero.
    ///
    /// `false` preserves the existing limit.
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

/// CPU bandwidth limit.
///
/// The default represents no quota with a 100000 microsecond period.
/// A zero period or explicit zero quota is invalid.
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

/// CPU, memory, and process limits applied to one execution scope.
///
/// At least one field must be set.
/// CPU periods and explicit quotas must be greater than zero.
/// Memory and process limits must be greater than zero.
#[derive(Debug, Clone, Default)]
pub struct CgroupLimits {
    /// CPU bandwidth limit.
    pub cpu: Option<CpuMax>,
    /// Maximum memory in bytes.
    pub memory: Option<u64>,
    /// Maximum number of processes and threads.
    pub pids: Option<u64>,
}

impl CgroupLimits {
    /// Returns `true` when no limit is configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.cpu.is_none() && self.memory.is_none() && self.pids.is_none()
    }
}

pub(crate) fn validate_cgroup_limits(limits: &CgroupLimits) -> Result<(), String> {
    if limits.is_empty() {
        return Err("cgroups configuration must contain at least one limit".into());
    }
    if let Some(cpu) = &limits.cpu {
        if cpu.period == 0 {
            return Err("cgroups.cpu.period cannot be zero".into());
        }
        if cpu.quota == Some(0) {
            return Err("cgroups.cpu.quota cannot be zero".into());
        }
    }
    if limits.memory == Some(0) {
        return Err("cgroups.memory cannot be zero".into());
    }
    if limits.pids == Some(0) {
        return Err("cgroups.pids cannot be zero".into());
    }
    Ok(())
}
