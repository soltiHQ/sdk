//! # Linux capabilities

/// Linux process capability.
///
/// The enum contains the capabilities supported by this crate.
/// It is non-exhaustive to permit additions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LinuxCapability {
    /// Changes file owners and groups (`CAP_CHOWN`).
    Chown,
    /// Bypasses discretionary file access checks (`CAP_DAC_OVERRIDE`).
    DacOverride,
    /// Bypasses file read and directory search checks (`CAP_DAC_READ_SEARCH`).
    DacReadSearch,
    /// Bypasses checks that require the file owner (`CAP_FOWNER`).
    FOwner,
    /// Preserves set-user-ID and set-group-ID bits (`CAP_FSETID`).
    FSetId,
    /// Bypasses signal permission checks (`CAP_KILL`).
    Kill,
    /// Changes process group ids (`CAP_SETGID`).
    SetGid,
    /// Changes process user ids (`CAP_SETUID`).
    SetUid,
    /// Changes process capabilities (`CAP_SETPCAP`).
    SetPCap,
    /// Binds sockets to privileged ports (`CAP_NET_BIND_SERVICE`).
    NetBindService,
    /// Uses raw and packet sockets (`CAP_NET_RAW`).
    NetRaw,
    /// Performs network administration (`CAP_NET_ADMIN`).
    NetAdmin,
    /// Changes the process root directory (`CAP_SYS_CHROOT`).
    SysChroot,
    /// Traces arbitrary processes (`CAP_SYS_PTRACE`).
    SysPtrace,
    /// Performs system administration operations (`CAP_SYS_ADMIN`).
    SysAdmin,
    /// Reboots the system or loads a new kernel (`CAP_SYS_BOOT`).
    SysBoot,
    /// Changes process scheduling and priority (`CAP_SYS_NICE`).
    SysNice,
    /// Overrides resource limits (`CAP_SYS_RESOURCE`).
    SysResource,
    /// Sets system and hardware clocks (`CAP_SYS_TIME`).
    SysTime,
    /// Creates special files (`CAP_MKNOD`).
    MkNod,
    /// Writes kernel audit records (`CAP_AUDIT_WRITE`).
    AuditWrite,
    /// Controls kernel auditing (`CAP_AUDIT_CONTROL`).
    AuditControl,
    /// Sets file capabilities (`CAP_SETFCAP`).
    SetFCap,
}

impl LinuxCapability {
    /// Returns the kernel name without the `CAP_` prefix.
    ///
    /// ```
    /// use solti_exec::isolation::LinuxCapability;
    ///
    /// assert_eq!(LinuxCapability::NetAdmin.name(), "NET_ADMIN");
    /// ```
    pub fn name(self) -> &'static str {
        match self {
            Self::Chown => "CHOWN",
            Self::DacOverride => "DAC_OVERRIDE",
            Self::DacReadSearch => "DAC_READ_SEARCH",
            Self::FOwner => "FOWNER",
            Self::FSetId => "FSETID",
            Self::Kill => "KILL",
            Self::SetGid => "SETGID",
            Self::SetUid => "SETUID",
            Self::SetPCap => "SETPCAP",
            Self::NetBindService => "NET_BIND_SERVICE",
            Self::NetRaw => "NET_RAW",
            Self::NetAdmin => "NET_ADMIN",
            Self::SysChroot => "SYS_CHROOT",
            Self::SysPtrace => "SYS_PTRACE",
            Self::SysAdmin => "SYS_ADMIN",
            Self::SysBoot => "SYS_BOOT",
            Self::SysNice => "SYS_NICE",
            Self::SysResource => "SYS_RESOURCE",
            Self::SysTime => "SYS_TIME",
            Self::MkNod => "MKNOD",
            Self::AuditWrite => "AUDIT_WRITE",
            Self::AuditControl => "AUDIT_CONTROL",
            Self::SetFCap => "SETFCAP",
        }
    }
}
