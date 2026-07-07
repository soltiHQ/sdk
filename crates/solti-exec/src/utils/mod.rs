//! # Utils: subprocess backend components.
//!
//! Low-level building blocks applied to child processes via `pre_exec` hooks.
//! Each module handles one isolation concern and follows the same conventions:
//!
//! - **Async-signal safety**: `pre_exec` closures capture only `Copy` types, call only raw syscalls
//! - **Declarative config struct** with `is_empty()` guard (zero overhead when unused)
//! - **Per-operation error logging** via [`log::pre_exec_log`] (no heap in the child)
//!
//! ## Modules
//!
//! | Module         | Config              | What it does                         |
//! |----------------|---------------------|--------------------------------------|
//! | [`cgroups`]    | [`CgroupLimits`]    | cgroup v2 limits (CPU, memory, PIDs) |
//! | [`limits`]     | [`RlimitConfig`]    | POSIX rlimits (nofile, fsize, core)  |
//! | [`security`]   | [`SecurityConfig`]  | capability drop + `no_new_privs`     |
//! | [`capability`] | [`LinuxCapability`] | capability enum with kernel values   |
//! | [`log`]        | —                   | async-signal-safe stderr logging     |

mod cgroups;
pub use cgroups::{CgroupLimits, CpuMax};
pub(crate) use cgroups::{attach_cgroup, build_cgroup_name, cleanup_cgroup, prepare_cgroup};

mod limits;
pub use limits::RlimitConfig;
pub(crate) use limits::attach_rlimits;

mod security;
pub(crate) use security::attach_security;
pub use security::{Namespaces, SeccompPolicy, SecurityConfig};

mod capability;
pub use capability::LinuxCapability;

pub(crate) mod log;
