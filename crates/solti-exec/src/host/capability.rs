use crate::isolation::LinuxCapability;

#[cfg(all(feature = "subprocess", target_os = "linux"))]
use std::io;

#[cfg(all(feature = "subprocess", target_os = "linux"))]
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[cfg(all(feature = "subprocess", target_os = "linux"))]
#[repr(C)]
struct CapUserHeader {
    version: u32,
    pid: libc::c_int,
}

#[cfg(all(feature = "subprocess", target_os = "linux"))]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapUserData {
    effective: u32,
    _permitted: u32,
    _inheritable: u32,
}

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

/// Returns whether the current thread has one effective Linux capability.
#[cfg(all(feature = "subprocess", target_os = "linux"))]
pub(crate) fn current_thread_has_effective_capability(
    capability: LinuxCapability,
) -> io::Result<bool> {
    let mut header = CapUserHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapUserData::default(); 2];

    // SAFETY:
    // the structs match Linux capability ABI version 3 and point to writable storage.
    if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let value = capability.to_cap_value();
    let index = (value / 32) as usize;
    Ok(index < data.len() && data[index].effective & (1 << (value % 32)) != 0)
}
