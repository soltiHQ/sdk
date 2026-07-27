//! Process limits and Linux security used by the subprocess backend.
//!
//! Configuration is prepared in the parent. Child hooks perform only the
//! syscalls required between `fork` and `execve`.

mod cgroups;
pub use cgroups::{CgroupLimits, CpuMax};
pub(crate) use cgroups::{
    PreparedCgroup, attach_cgroup, build_cgroup_name, cleanup_cgroup, prepare_cgroup,
    resolve_cgroup_parent,
};

mod limits;
pub use limits::RlimitConfig;
pub(crate) use limits::attach_rlimits;

mod security;
pub(crate) use security::attach_security;
pub use security::{Namespaces, SeccompPolicy, SecurityConfig};

mod capability;
pub use capability::LinuxCapability;

pub(crate) mod log;
