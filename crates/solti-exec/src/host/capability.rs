use crate::isolation::LinuxCapability;

impl LinuxCapability {
    /// Returns the value from `<linux/capability.h>`.
    #[cfg(all(feature = "host-process", target_os = "linux"))]
    pub(crate) fn to_cap_value(self) -> u32 {
        match self {
            Self::Chown => 0,           // CAP_CHOWN
            Self::DacOverride => 1,     // CAP_DAC_OVERRIDE
            Self::DacReadSearch => 2,   // CAP_DAC_READ_SEARCH
            Self::FOwner => 3,          // CAP_FOWNER
            Self::FSetId => 4,          // CAP_FSETID
            Self::Kill => 5,            // CAP_KILL
            Self::SetGid => 6,          // CAP_SETGID
            Self::SetUid => 7,          // CAP_SETUID
            Self::SetPCap => 8,         // CAP_SETPCAP
            Self::NetBindService => 10, // CAP_NET_BIND_SERVICE
            Self::NetAdmin => 12,       // CAP_NET_ADMIN
            Self::NetRaw => 13,         // CAP_NET_RAW
            Self::SysChroot => 18,      // CAP_SYS_CHROOT
            Self::SysPtrace => 19,      // CAP_SYS_PTRACE
            Self::SysAdmin => 21,       // CAP_SYS_ADMIN
            Self::SysBoot => 22,        // CAP_SYS_BOOT
            Self::SysNice => 23,        // CAP_SYS_NICE
            Self::SysResource => 24,    // CAP_SYS_RESOURCE
            Self::SysTime => 25,        // CAP_SYS_TIME
            Self::MkNod => 27,          // CAP_MKNOD
            Self::AuditWrite => 29,     // CAP_AUDIT_WRITE
            Self::AuditControl => 30,   // CAP_AUDIT_CONTROL
            Self::SetFCap => 31,        // CAP_SETFCAP
        }
    }
}
