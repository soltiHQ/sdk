//! # Host process controls
//!
//! [`HostProcessPolicy`] describes controls for an operating-system process.
//!
//! The policy is declarative.
//! Backends prepare it before process creation.
//! Child hooks apply enabled controls between `fork` and `execve`.
//! Cgroups are removed after the process scope has stopped.
//!
//! Current controls provide process hardening.
//! They do not form a complete sandbox for untrusted code.

mod capability;
pub use crate::isolation::LinuxCapability;
#[cfg(all(feature = "subprocess", target_os = "linux"))]
pub(crate) use capability::current_thread_has_effective_capability;

mod cgroups;
pub use crate::isolation::{CgroupLimits, CpuMax};
pub use cgroups::DomainTermination;
#[cfg(feature = "host-process")]
pub(crate) use cgroups::{
    CgroupDomain, CgroupPrepareFailure, PreparedCgroup, PreparedCgroupParent, attach_cgroup,
    prepare_cgroup_owned, resolve_cgroup_parent,
};

#[cfg(feature = "host-process")]
mod error;
#[cfg(feature = "host-process")]
pub use error::HostProcessError;

#[cfg(all(feature = "subprocess", unix))]
mod fds;
#[cfg(all(feature = "subprocess", unix))]
pub(crate) use fds::attach_fd_cloexec;

mod limits;
pub use crate::isolation::RlimitConfig;
#[cfg(feature = "host-process")]
pub use limits::PreparedRlimits;
#[cfg(feature = "host-process")]
pub(crate) use limits::attach_rlimits;
#[cfg(all(test, feature = "host-process", unix))]
pub(crate) use limits::reduced_nofile_limit_for_test;

#[cfg(all(feature = "host-process", unix))]
pub(crate) mod log;

mod process;
pub use process::ProcessConfig;
#[cfg(feature = "host-process")]
pub(crate) use process::{PreparedProcessConfig, attach_process_config};

mod policy;
#[cfg(all(feature = "host-process", feature = "subprocess"))]
pub(crate) use policy::AttemptPrepareFailure;
pub use policy::{
    AttemptProcessDomain, HostProcessPolicy, PreparedHostProcessAttempt, PreparedHostProcessPolicy,
};

mod security;
pub use crate::isolation::{ProcessCredentials, SeccompPolicy};
#[cfg(feature = "host-process")]
pub(crate) use security::attach_security;
pub use security::{Namespaces, SecurityConfig};
