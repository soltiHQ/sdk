//! # Process isolation values
//!
//! These types describe low-level process controls.
//! Host and container backends translate them into their native APIs.

mod capability;
pub use capability::LinuxCapability;

mod credentials;
pub use credentials::ProcessCredentials;
pub(crate) use credentials::validate_credentials;

mod resources;
pub(crate) use resources::validate_cgroup_limits;
pub use resources::{CgroupLimits, CpuMax, RlimitConfig};

mod seccomp;
pub use seccomp::SeccompPolicy;
#[cfg(any(feature = "containerd", all(feature = "seccomp", target_os = "linux")))]
pub(crate) use seccomp::deny_host_control_syscalls;

pub(crate) fn validate_umask(mask: u32) -> Result<(), String> {
    if mask & !0o777 != 0 {
        return Err("process.umask may contain only permission bits 0o000..=0o777".into());
    }
    Ok(())
}
