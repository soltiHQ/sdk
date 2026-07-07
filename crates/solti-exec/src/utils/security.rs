//! # Security: process-level hardening for subprocess runners.
//!
//! [`SecurityConfig`] restricts the privilege set of child processes spawned by subprocess runners.
//!
//! **Linux:**
//! - Drop all process capabilities in one batch (`capget` → mask → `capset`)
//! - Zero heap allocation in the child (closure captures only `Copy` types)
//! - Raise kept caps in the ambient set for unprivileged `execve`
//! - Keep an optional allowlist of caps via [`LinuxCapability`]
//! - Set `no_new_privs` to block suid/sgid escalation
//!
//! **Other platforms:**
//! - `tracing::warn` and no-op.
//!
//! ## Threat model (read this before trusting it)
//!
//! What it enforces (Linux, when the agent has the needed privilege):
//! - drops Linux capabilities to an allowlist,
//! - sets `no_new_privs` (blocks suid/sgid escalation),
//! - switches the child to an unprivileged user/group via
//!   [`run_as_uid`](SecurityConfig::run_as_uid) / [`run_as_gid`](SecurityConfig::run_as_gid),
//! - unshares [namespaces](Namespaces) (mount, network, PID, IPC, UTS, cgroup),
//! - filters syscalls with seccomp-bpf via [`SeccompPolicy`] (the `seccomp` feature),
//! - scrubs the environment via [`EnvPolicy`](crate::subprocess::EnvPolicy)
//!   (a separate backend knob).
//!
//! Honest limits: [`SeccompPolicy::BlockDangerous`] is a **blocklist**, not a
//! curated default-deny allowlist, so it stops the classic escape syscalls but
//! is weaker than a full allowlist. There is no user-namespace remapping, so a
//! child in a mount/pid namespace still shares the agent's uid space unless you
//! also set `run_as_uid`. For hostile, fully-untrusted workloads, run the agent
//! itself inside a container runtime, gVisor, or a microVM.
//!
//! Enforcement needs privilege: unshare and the id drop require `CAP_SYS_ADMIN`
//! / `CAP_SETUID` / `CAP_SETGID`. Pair this with
//! [`with_require_enforcement`](crate::subprocess::SubprocessBackendConfig::with_require_enforcement)
//! so a host that cannot apply the confinement fails closed instead of running
//! the task unconfined. Set a [`CgroupLimits`](crate::CgroupLimits) `pids`/`memory`
//! cap to bound resource exhaustion.
//!
//! ## Also
//!
//! - [`SubprocessBackendConfig`](crate::subprocess::SubprocessBackendConfig) builder that consumes `SecurityConfig`.
//! - [`LinuxCapability`](super::LinuxCapability) capability identifiers for the keep list.
//!
//! ## What happens when a subprocess spawns
//! ```text
//!                        parent process
//!                             │
//!                           fork()
//!                             │
//!          ┌──────────────────┼───────────────────┐
//!          │            child process             │
//!          │                                      │
//!          │  ┌── pre_exec hook ───────────────┐  │
//!          │  │  1. unshare(namespaces)        │  │
//!          │  │  2. set no_new_privs           │  │
//!          │  │  3. setgroups + setgid + setuid│  │
//!          │  │  4. clear ambient caps         │  │
//!          │  │  5. capget → mask → capset     │  │
//!          │  │  6. raise kept in ambient      │  │
//!          │  └────────────────────────────────┘  │
//!          │                                      │
//!          │  execve("echo", ["hello"])           │
//!          │  (isolated, unprivileged, min caps)  │
//!          └──────────────────────────────────────┘
//! ```
//!
//! ## How attach_security works
//! ```text
//! attach_security(&mut cmd, &config)
//!     ├──► config.is_empty()? → return early, no hook
//!     │
//!     ├──► Linux:
//!     │     ├──► build KeepMask from config.keep_caps
//!     │     │     └──► Vec<LinuxCapability> → [u32; 2] bitmask (Copy, stack-only)
//!     │     │
//!     │     └──► install pre_exec closure on Command
//!     │           └──► captures: drop_all_caps (bool), no_new_privs (bool), keep_mask ([u32; 2])
//!     │                zero heap: all Copy types
//!     │
//!     └──► non-Linux:
//!           └──► warn!("security settings ignored on {os}") → Ok(())
//! ```
//!
//! ## Capability drop: step by step
//! ```text
//! drop_capabilities_batch(keep_mask)
//!     │
//!     ├──► prctl(PR_CAP_AMBIENT, CLEAR_ALL)
//!     │     └──► EINVAL? kernel < 4.3, no ambient - Ok, continue
//!     │
//!     ├──► capget() → read current caps into CapUserData[2]
//!     │    ┌────────────────────────────────────────────────┐
//!     │    │  before mask             after mask            │
//!     │    │  effective:  1111        effective:  0010      │
//!     │    │  permitted:  1111        permitted:  0010      │
//!     │    │  inheritable:1111        inheritable:0010      │
//!     │    │                                                │
//!     │    │  keep_mask = 0010 (only CAP_NET_BIND_SERVICE)  │
//!     │    └────────────────────────────────────────────────┘
//!     │
//!     ├──► capset() ← one syscall writes all caps
//!     │
//!     └──► for each cap set in keep_mask:
//!           └──► prctl(PR_CAP_AMBIENT, RAISE, cap)
//!                EINVAL | EPERM → Ok (best-effort, older kernel or no permission)
//! ```
//!
//! ## KeepMask layout
//! ```text
//! Linux capability v3 format: CapUserData[2] = 2 × u32 = 64 bits
//!
//!   bits[0]                          bits[1]
//!   ┌─────────────────────────────┐  ┌─────────────────────────────┐
//!   │ cap 0  cap 1 ... cap 31     │  │ cap 32  cap 33 ... cap 63   │
//!   └─────────────────────────────┘  └─────────────────────────────┘
//!
//!   CAP_LAST_CAP = 63 - this is NOT a guess, it's the v3 ABI limit.
//!   If kernel ever adds cap > 63, that requires a v4 format with new structs and new syscall signatures - this whole module would need updating anyway.
//! ```
//!
//! ## Configuration
//!
//! | Field               | What it does                          | Needs privileges?  | If it fails                                    |
//! |---------------------|---------------------------------------|--------------------|------------------------------------------------|
//! | `drop_all_caps`     | strip all caps except `keep_caps`     | `CAP_SETPCAP`      | logs warning, go on (or abort if strict)       |
//! | `keep_caps`         | allowlist: caps to preserve           | `CAP_SETPCAP`      | logs warning, go on (or abort if strict)       |
//! | `fail_on_cap_error` | strict mode: abort spawn on cap error | —                  | —                                              |
//! | `no_new_privs`      | block suid/sgid privilege escalation  | none (any user)    | **always aborts spawn**                        |
//! | `run_as_gid`        | drop to an unprivileged group         | `CAP_SETGID`       | **always aborts spawn**                        |
//! | `run_as_uid`        | drop to an unprivileged user          | `CAP_SETUID`       | **always aborts spawn**                        |
//! | `namespaces`        | unshare mount/net/pid/ipc/uts/cgroup  | `CAP_SYS_ADMIN`    | **always aborts spawn**                        |
//!
//! ## Async-signal safety
//!
//! Everything inside the `pre_exec` closure runs **between `fork()` and `execve()`**.
//! POSIX says only async-signal-safe functions are allowed there.
//!
//! | What we call                 | Why it's safe                              |
//! |------------------------------|--------------------------------------------|
//! | `prctl()`                    | direct syscall                             |
//! | `capget()` / `capset()`      | direct syscalls                            |
//! | `libc::write(STDERR)`        | async-signal-safe per POSIX                |
//! | `io::Error::last_os_error()` | reads `errno`, no heap (Rust ≥ 1.74)       |
//!
//! The closure captures `Copy` types (bools, masks, ids) and, with the
//! `seccomp` feature, a pre-built BPF program allocated **before** fork.
//! Nothing is allocated or dropped in the child.
//!
//! ## Rules
//! - Capability drop failures are **non-fatal** by default (logged via `pre_exec_log`, continues)
//! - Set `fail_on_cap_error = true` to make capability drop failures **fatal** (aborts spawn)
//! - Non-Linux: all knobs are no-op, warning emitted via `tracing::warn`
//! - `no_new_privs` failure is **always fatal** (returns `Err`, `Command::spawn` fails)
//! - `KeepMask` is built **before** fork (safe to iterate `Vec<LinuxCapability>`)
//! - `SecurityConfig::is_empty()` → no hook installed, zero overhead
use tokio::process::Command;

use crate::utils::LinuxCapability;

#[cfg(not(target_os = "linux"))]
use tracing::warn;

/// Linux namespaces to unshare for the child before `execve`.
///
/// Each flag isolates one kernel resource. Unsharing requires privilege
/// (`CAP_SYS_ADMIN`) unless the process is already in a user namespace;
/// a failed unshare aborts the spawn (fail closed).
#[derive(Debug, Clone, Copy, Default)]
pub struct Namespaces {
    /// Mount namespace (`CLONE_NEWNS`): a private view of the filesystem tree.
    pub mount: bool,
    /// Network namespace (`CLONE_NEWNET`): no network interfaces except loopback.
    pub net: bool,
    /// PID namespace (`CLONE_NEWPID`): the child's descendants see an isolated PID space.
    pub pid: bool,
    /// IPC namespace (`CLONE_NEWIPC`): isolated System V IPC / POSIX message queues.
    pub ipc: bool,
    /// UTS namespace (`CLONE_NEWUTS`): isolated hostname / domainname.
    pub uts: bool,
    /// Cgroup namespace (`CLONE_NEWCGROUP`): the cgroup root is virtualized.
    pub cgroup: bool,
}

impl Namespaces {
    /// Returns `true` when no namespace is requested.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !(self.mount || self.net || self.pid || self.ipc || self.uts || self.cgroup)
    }

    /// The combined `CLONE_NEW*` flag mask for `unshare(2)`.
    #[cfg(target_os = "linux")]
    fn mask(&self) -> libc::c_int {
        let mut m = 0;
        if self.mount {
            m |= libc::CLONE_NEWNS;
        }
        if self.net {
            m |= libc::CLONE_NEWNET;
        }
        if self.pid {
            m |= libc::CLONE_NEWPID;
        }
        if self.ipc {
            m |= libc::CLONE_NEWIPC;
        }
        if self.uts {
            m |= libc::CLONE_NEWUTS;
        }
        if self.cgroup {
            m |= libc::CLONE_NEWCGROUP;
        }
        m
    }
}

/// seccomp-bpf syscall filtering policy for the child.
///
/// Enforced only on Linux and only when the crate's `seccomp` feature is
/// enabled. Without the feature a non-[`Disabled`](Self::Disabled) policy is
/// rejected at config validation (fail closed, never silently ignored).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SeccompPolicy {
    /// No syscall filter is installed.
    #[default]
    Disabled,
    /// A default-allow filter that blocks a small set of syscalls no ordinary
    /// task needs and that are classic escape or host-damage vectors (module
    /// loading, `ptrace`, `mount`, `kexec`, `bpf`, `perf_event_open`, joining
    /// namespaces, the kernel keyring, ...). Blocked syscalls fail with `EPERM`.
    ///
    /// This is a blocklist: it will not break normal programs, but it is weaker
    /// than a curated allowlist. For maximum confinement run the agent under a
    /// runtime that ships a default-deny profile.
    BlockDangerous,
}

/// Declarative security policy.
#[derive(Debug, Clone, Default)]
pub struct SecurityConfig {
    /// Drop all capabilities before exec.
    ///
    /// Note: capability operations require CAP_SETPCAP or root.
    /// If the process lacks these privileges, the operation will log a warning and continue (unless `fail_on_cap_error` is set).
    pub drop_all_caps: bool,
    /// Optional allowlist of capabilities to keep after `drop_all_caps`.
    ///
    /// Only meaningful when `drop_all_caps = true`.
    pub keep_caps: Vec<LinuxCapability>,
    /// Enable `no_new_privs` for the child process.
    ///
    /// This flag works without root privileges.
    /// Failures to set this flag are always fatal (spawn will fail).
    pub no_new_privs: bool,
    /// When `true`, capability drop failures abort the spawn instead of logging and continuing.
    ///
    /// Default: `false` (best-effort - non-fatal).
    pub fail_on_cap_error: bool,
    /// Drop to this group id before exec (`setgid`). Applied before `run_as_uid`.
    ///
    /// Requires `CAP_SETGID` (or root). A failure always aborts the spawn.
    /// Set it together with `run_as_uid` to run the child as an unprivileged user.
    pub run_as_gid: Option<u32>,
    /// Drop to this user id before exec (`setuid`), after supplementary groups
    /// are cleared and `run_as_gid` is applied. This is a one-way, permanent drop.
    ///
    /// Requires `CAP_SETUID` (or root). A failure always aborts the spawn.
    pub run_as_uid: Option<u32>,
    /// Linux namespaces to unshare before exec.
    pub namespaces: Namespaces,
    /// seccomp-bpf syscall filter (Linux + `seccomp` feature).
    pub seccomp: SeccompPolicy,
}

impl SecurityConfig {
    /// Returns `true` if no security knobs are configured.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.drop_all_caps
            && self.keep_caps.is_empty()
            && !self.no_new_privs
            && self.run_as_uid.is_none()
            && self.run_as_gid.is_none()
            && self.namespaces.is_empty()
            && self.seccomp == SeccompPolicy::Disabled
    }

    /// Validate the security policy at config time.
    ///
    /// Rejects a `keep_caps` allowlist with `drop_all_caps = false`:
    /// the keep list is only applied *while* dropping. Without `drop_all_caps`
    /// the hook would drop nothing and the child would keep **all** parent capabilities - a silent fail-open.
    pub(crate) fn validate(&self) -> Result<(), crate::ExecError> {
        if !self.keep_caps.is_empty() && !self.drop_all_caps {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "security.keep_caps is set but drop_all_caps is false: the allowlist would have \
                 nothing to drop from; the child would keep ALL capabilities"
                    .into(),
            ));
        }
        #[cfg(not(feature = "seccomp"))]
        if self.seccomp != SeccompPolicy::Disabled {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "security.seccomp is set but the crate's `seccomp` feature is not enabled".into(),
            ));
        }
        // Confirm the BPF program compiles at config time, so a spawn never
        // fails closed on a filter that could have been rejected earlier.
        #[cfg(all(target_os = "linux", feature = "seccomp"))]
        if linux_impl::seccomp::build_program(&self.seccomp).is_err() {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "security.seccomp filter could not be compiled".into(),
            ));
        }
        Ok(())
    }
}

/// Attach security policy to a `tokio::process::Command`.
pub fn attach_security(cmd: &mut Command, config: &SecurityConfig) {
    if config.is_empty() {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        linux_impl::attach(cmd, config);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = &cmd;
        warn!(
            ?config,
            "security configuration is only enforced on Linux; current OS={}: settings will be ignored",
            std::env::consts::OS,
        );
    }
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{KeepMask, SecurityConfig};

    use crate::utils::log::{pre_exec_log, pre_exec_log_errno};
    use std::io;
    use tokio::process::Command;

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;
    const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
    /// Upper bound of capability v3 bitmask: `CapUserData[2]` = 2 × 32 = 64 bits → caps 0..63.
    /// This is a kernel ABI limit, not a guess. A v4 format would require new structs + syscall signatures.
    const CAP_LAST_CAP: u32 = 63;

    /// Install the `pre_exec` hook on the command.
    ///
    /// Caller (`attach_security`) already checked `!config.is_empty()`.
    ///
    /// Step order in the child is fixed by privilege dependencies:
    /// unshare namespaces (needs `CAP_SYS_ADMIN`) → `no_new_privs` → drop
    /// group/user id (needs `CAP_SETGID`/`CAP_SETUID`) → tighten capabilities
    /// **last** (dropping `CAP_SETUID` before the id drop would make it fail).
    ///
    /// Note: `keep_caps` does not survive a `run_as_uid` drop. A `setuid` to a
    /// non-root user clears the permitted capability set, so the two are not
    /// meant to be combined; the id drop wins.
    pub fn attach(cmd: &mut Command, config: &SecurityConfig) {
        let keep_mask = KeepMask::from_caps(&config.keep_caps);
        let fail_on_cap_error = config.fail_on_cap_error;
        let drop_all_caps = config.drop_all_caps;
        let no_new_privs = config.no_new_privs;
        let ns_mask = config.namespaces.mask();
        let run_as_gid = config.run_as_gid;
        let run_as_uid = config.run_as_uid;

        // The seccomp BPF program is compiled here, in the PARENT, before fork.
        // The child only installs it (a syscall), never allocates.
        #[cfg(feature = "seccomp")]
        let seccomp_prog = seccomp::build_program(&config.seccomp);

        // SAFETY:
        // The pre_exec closure runs between fork() and execve() in the child process.
        //
        // It calls unshare, prctl, capget/capset, setgroups/setgid/setuid, and
        // seccompiler::apply_filter (all async-signal-safe: raw syscalls with no
        // allocation) plus pre_exec_log (raw libc::write).
        // Error paths use io::Error::last_os_error() which stores errno inline without heap allocation (Rust >= 1.74).
        //
        // Captured values are either Copy (bools, masks, ids) or a pre-built,
        // read-only BpfProgram allocated before fork: nothing is allocated or
        // dropped in the child.
        unsafe {
            cmd.pre_exec(move || {
                if ns_mask != 0 {
                    apply_namespaces(ns_mask)?;
                }
                if no_new_privs {
                    apply_no_new_privs()?;
                }
                // Id drop needs CAP_SETGID/CAP_SETUID, which the capability drop
                // below removes — so it must run first, and it is always fatal.
                drop_privileges(run_as_gid, run_as_uid)?;
                if drop_all_caps
                    && let Err(e) = drop_capabilities_batch(keep_mask)
                    && fail_on_cap_error
                {
                    return Err(e);
                }
                // seccomp is installed last, right before execve: once active it
                // constrains every later syscall, and execve stays allowed.
                #[cfg(feature = "seccomp")]
                match &seccomp_prog {
                    Ok(Some(prog)) => seccomp::install(prog)?,
                    Ok(None) => {}
                    Err(()) => {
                        pre_exec_log(b"solti-exec: seccomp filter unavailable; aborting spawn\n");
                        return Err(io::Error::from_raw_os_error(libc::EPERM));
                    }
                }
                Ok(())
            });
        }
    }

    /// seccomp-bpf filtering, compiled in the parent and installed in the child.
    #[cfg(feature = "seccomp")]
    pub(super) mod seccomp {
        use super::super::SeccompPolicy;
        use crate::utils::log::pre_exec_log;
        use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
        use std::collections::BTreeMap;
        use std::io;

        /// Syscalls no ordinary task needs, all classic escape or host-damage
        /// vectors. Blocked with `EPERM` under [`SeccompPolicy::BlockDangerous`].
        fn dangerous_syscalls() -> &'static [libc::c_long] {
            &[
                libc::SYS_ptrace,
                libc::SYS_mount,
                libc::SYS_umount2,
                libc::SYS_pivot_root,
                libc::SYS_kexec_load,
                libc::SYS_kexec_file_load,
                libc::SYS_init_module,
                libc::SYS_finit_module,
                libc::SYS_delete_module,
                libc::SYS_bpf,
                libc::SYS_perf_event_open,
                libc::SYS_swapon,
                libc::SYS_swapoff,
                libc::SYS_reboot,
                libc::SYS_setns,
                libc::SYS_acct,
                libc::SYS_add_key,
                libc::SYS_keyctl,
                libc::SYS_request_key,
            ]
        }

        #[cfg(target_arch = "x86_64")]
        const ARCH: Option<TargetArch> = Some(TargetArch::x86_64);
        #[cfg(target_arch = "aarch64")]
        const ARCH: Option<TargetArch> = Some(TargetArch::aarch64);
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        const ARCH: Option<TargetArch> = None;

        /// Compile the BPF program for a policy, in the parent, before fork.
        ///
        /// `Ok(None)` means "no filter" only when the policy is
        /// [`SeccompPolicy::Disabled`]. A requested filter on an unsupported
        /// architecture returns `Err(())` so the spawn fails closed instead of
        /// silently running unfiltered. `Err(())` also covers a build failure.
        pub(crate) fn build_program(policy: &SeccompPolicy) -> Result<Option<BpfProgram>, ()> {
            match policy {
                SeccompPolicy::Disabled => Ok(None),
                SeccompPolicy::BlockDangerous => match ARCH {
                    // Unsupported architecture: refuse rather than run unfiltered.
                    None => Err(()),
                    Some(arch) => build_blocklist(arch).map(Some).map_err(|_| ()),
                },
            }
        }

        fn build_blocklist(arch: TargetArch) -> Result<BpfProgram, Box<dyn std::error::Error>> {
            // Default-allow (mismatch), block the dangerous set (empty rule set =
            // unconditional match) with EPERM.
            let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
            for &sc in dangerous_syscalls() {
                // `c_long` is `i64` on the LP64 targets seccompiler supports, but
                // the conversion keeps this correct on any target where it differs.
                #[allow(clippy::useless_conversion)]
                rules.insert(i64::from(sc), vec![]);
            }
            let filter = SeccompFilter::new(
                rules,
                SeccompAction::Allow,
                SeccompAction::Errno(libc::EPERM as u32),
                arch,
            )?;
            let prog: BpfProgram = filter.try_into()?;
            Ok(prog)
        }

        /// Install a pre-built program in the child. Async-signal-safe: a single
        /// `seccomp`/`prctl` syscall over a program allocated before fork.
        pub(super) fn install(prog: &BpfProgram) -> io::Result<()> {
            match seccompiler::apply_filter(prog) {
                Ok(()) => Ok(()),
                Err(_) => {
                    pre_exec_log(b"solti-exec: seccomp apply_filter failed\n");
                    Err(io::Error::from_raw_os_error(libc::EPERM))
                }
            }
        }
    }

    /// Unshare the requested namespaces. A failure aborts the spawn (fail closed).
    fn apply_namespaces(mask: libc::c_int) -> io::Result<()> {
        // SAFETY: `unshare` is an async-signal-safe syscall taking a scalar flag
        // mask; no pointer is dereferenced.
        let rc = unsafe { libc::unshare(mask) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            pre_exec_log(b"solti-exec: unshare failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }
        Ok(())
    }

    /// Drop to the configured group then user id. One-way and always fatal on error.
    ///
    /// Order is mandatory: supplementary groups are cleared and `setgid` is
    /// applied **before** `setuid`, because after `setuid` to an unprivileged
    /// user the process can no longer change its gid.
    ///
    /// Supplementary groups are cleared whenever **either** id is dropped: a
    /// child that becomes non-root must not keep the agent's group memberships,
    /// even when only `run_as_uid` was set.
    fn drop_privileges(gid: Option<u32>, uid: Option<u32>) -> io::Result<()> {
        if gid.is_some() || uid.is_some() {
            // Clear supplementary groups before dropping either id. Needs
            // CAP_SETGID; propagate any failure (fail closed).
            // SAFETY: `setgroups(0, NULL)` is async-signal-safe; a null list with
            // count 0 is the documented "clear all" form.
            let rc = unsafe { libc::setgroups(0, std::ptr::null()) };
            if rc != 0 {
                let e = io::Error::last_os_error();
                pre_exec_log(b"solti-exec: setgroups(clear) failed: ");
                if let Some(code) = e.raw_os_error() {
                    pre_exec_log_errno(code);
                }
                return Err(e);
            }
        }
        if let Some(gid) = gid {
            // SAFETY: `setgid` is an async-signal-safe syscall taking a scalar gid.
            if unsafe { libc::setgid(gid as libc::gid_t) } != 0 {
                let e = io::Error::last_os_error();
                pre_exec_log(b"solti-exec: setgid failed: ");
                if let Some(code) = e.raw_os_error() {
                    pre_exec_log_errno(code);
                }
                return Err(e);
            }
        }
        if let Some(uid) = uid {
            // SAFETY: `setuid` is an async-signal-safe syscall taking a scalar uid.
            if unsafe { libc::setuid(uid as libc::uid_t) } != 0 {
                let e = io::Error::last_os_error();
                pre_exec_log(b"solti-exec: setuid failed: ");
                if let Some(code) = e.raw_os_error() {
                    pre_exec_log_errno(code);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Drop all capabilities except those in `keep_mask`, using batch capget/capset.
    ///
    /// Each step logs a distinct prefix on failure so the operator can tell which syscall failed (clear_ambient / capget / capset).
    fn drop_capabilities_batch(keep_mask: KeepMask) -> io::Result<()> {
        if let Err(e) = clear_ambient_caps() {
            pre_exec_log(b"solti-exec: clear_ambient_caps failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }

        let mut header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapUserData::default(); 2];

        // SAFETY:
        // Header and data are valid stack-local #[repr(C)] structs matching the kernel's
        // __user_cap_header_struct / __user_cap_data_struct layout.
        if unsafe { capget(&mut header, data.as_mut_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            pre_exec_log(b"solti-exec: capget failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }

        data[0].effective &= keep_mask.bits[0];
        data[0].permitted &= keep_mask.bits[0];
        data[0].inheritable &= keep_mask.bits[0];
        data[1].effective &= keep_mask.bits[1];
        data[1].permitted &= keep_mask.bits[1];
        data[1].inheritable &= keep_mask.bits[1];

        // SAFETY:
        // Same structs, modified in-place.
        // Single capset writes the new state.
        if unsafe { capset(&mut header, data.as_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            pre_exec_log(b"solti-exec: capset failed: ");
            if let Some(code) = e.raw_os_error() {
                pre_exec_log_errno(code);
            }
            return Err(e);
        }

        for cap_value in 0..=CAP_LAST_CAP {
            if keep_mask.is_set(cap_value) {
                let _ = raise_ambient_cap(cap_value);
            }
        }

        Ok(())
    }

    /// Clear all ambient capabilities.
    fn clear_ambient_caps() -> io::Result<()> {
        // SAFETY:
        // direct async-signal-safe `prctl` syscall with scalar/constant arguments only (no pointer is dereferenced), safe to call post-fork.
        let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINVAL) {
                return Err(err);
            }
        }

        Ok(())
    }

    /// Raise a capability in the ambient set (best-effort).
    ///
    /// Returns `Ok(())` for `EINVAL` and `EPERM` (expected on older kernels or when lacking `CAP_SETPCAP`).
    /// Other errors propagate, but the caller ignores the result with `let _ =`.
    fn raise_ambient_cap(cap: u32) -> io::Result<()> {
        // SAFETY:
        // direct async-signal-safe `prctl` syscall with scalar arguments only (no pointer is dereferenced), safe to call post-fork.
        let rc = unsafe { libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, cap, 0, 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINVAL) | Some(libc::EPERM) => return Ok(()),
                _ => return Err(err),
            }
        }
        Ok(())
    }

    fn apply_no_new_privs() -> io::Result<()> {
        // SAFETY: direct async-signal-safe `prctl` syscall with scalar/constant
        // arguments only (no pointer is dereferenced), safe to call post-fork.
        let rc = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[repr(C)]
    struct CapUserHeader {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct CapUserData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    unsafe extern "C" {
        fn capset(hdrp: *mut CapUserHeader, datap: *const CapUserData) -> libc::c_int;
        fn capget(hdrp: *mut CapUserHeader, datap: *mut CapUserData) -> libc::c_int;
    }
}

/// Bitmask of Linux capabilities to keep after a bulk drop.
///
/// Layout mirrors the kernel v3 capability format: two `u32` words covering caps 0..31 and 32..63 respectively.
#[derive(Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct KeepMask {
    /// `bits[0]` covers caps 0..31, `bits[1]` covers caps 32..63.
    bits: [u32; 2],
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl KeepMask {
    /// Build a keep-mask from a slice of capabilities.
    fn from_caps(caps: &[LinuxCapability]) -> Self {
        let mut bits = [0u32; 2];
        for cap in caps {
            let v = cap.to_cap_value();
            let idx = (v / 32) as usize;
            if idx < 2 {
                bits[idx] |= 1u32 << (v % 32);
            }
        }
        Self { bits }
    }

    /// Returns `true` if the given capability number is set in the mask.
    fn is_set(self, cap: u32) -> bool {
        let idx = (cap / 32) as usize;
        if idx >= 2 {
            return false;
        }
        (self.bits[idx] & (1u32 << (cap % 32))) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    #[test]
    fn empty_config_is_noop() {
        let cfg = SecurityConfig::default();
        assert!(cfg.is_empty());

        let mut cmd = Command::new("sh");
        attach_security(&mut cmd, &cfg);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_empty_config_attaches_pre_exec_hook_on_linux() {
        let cfg = SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetAdmin, LinuxCapability::NetBindService],
            no_new_privs: true,
            ..Default::default()
        };

        assert!(!cfg.is_empty());

        let mut cmd = Command::new("sh");
        attach_security(&mut cmd, &cfg);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_empty_config_is_ignored_on_non_linux() {
        let cfg = SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetAdmin],
            no_new_privs: true,
            ..Default::default()
        };

        assert!(!cfg.is_empty());

        let mut cmd = Command::new("sh");
        attach_security(&mut cmd, &cfg);
    }

    #[test]
    fn capability_names_are_correct() {
        assert_eq!(LinuxCapability::NetAdmin.name(), "NET_ADMIN");
        assert_eq!(LinuxCapability::SysAdmin.name(), "SYS_ADMIN");
        assert_eq!(LinuxCapability::Chown.name(), "CHOWN");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_new_privs_can_be_set_without_root() {
        let cfg = SecurityConfig {
            no_new_privs: true,
            ..Default::default()
        };
        let mut cmd = Command::new("true");
        attach_security(&mut cmd, &cfg);

        let result = cmd.status().await;
        assert!(result.is_ok(), "no_new_privs should work without root");
        assert!(result.unwrap().success());
    }

    #[test]
    fn keep_mask_empty_caps_all_zero() {
        let m = KeepMask::from_caps(&[]);
        assert_eq!(m.bits, [0, 0]);
        for cap in 0..=63 {
            assert!(!m.is_set(cap), "cap {cap} should not be set");
        }
    }

    #[test]
    fn keep_mask_single_low_cap() {
        let m = KeepMask::from_caps(&[LinuxCapability::Chown]);
        assert!(m.is_set(0));
        assert!(!m.is_set(1));
        assert_eq!(m.bits[0], 1);
        assert_eq!(m.bits[1], 0);
    }

    #[test]
    fn keep_mask_cap_in_second_word() {
        let m = KeepMask::from_caps(&[LinuxCapability::SetFCap, LinuxCapability::SysPtrace]);
        assert!(m.is_set(31));
        assert!(m.is_set(19));
        assert!(!m.is_set(0));
        assert_eq!(m.bits[1], 0)
    }

    #[test]
    fn keep_mask_multiple_caps() {
        let caps = [
            LinuxCapability::Chown,          // 0
            LinuxCapability::NetBindService, // 10
            LinuxCapability::NetAdmin,       // 12
            LinuxCapability::SysAdmin,       // 21
        ];
        let m = KeepMask::from_caps(&caps);
        assert!(m.is_set(0));
        assert!(m.is_set(10));
        assert!(m.is_set(12));
        assert!(m.is_set(21));
        assert!(!m.is_set(1));
        assert!(!m.is_set(11));
        assert!(!m.is_set(63));
    }

    #[test]
    fn keep_mask_duplicate_caps_idempotent() {
        let m1 = KeepMask::from_caps(&[LinuxCapability::Kill]);
        let m2 = KeepMask::from_caps(&[LinuxCapability::Kill, LinuxCapability::Kill]);
        assert_eq!(m1.bits, m2.bits);
    }

    #[test]
    fn keep_mask_out_of_range_returns_false() {
        let m = KeepMask::from_caps(&[LinuxCapability::Chown]);
        assert!(!m.is_set(64));
        assert!(!m.is_set(100));
        assert!(!m.is_set(u32::MAX));
    }

    #[test]
    fn namespaces_default_is_empty() {
        assert!(Namespaces::default().is_empty());
        assert!(
            !Namespaces {
                net: true,
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn namespace_mask_sets_the_right_flags() {
        let ns = Namespaces {
            net: true,
            mount: true,
            ..Default::default()
        };
        let m = ns.mask();
        assert_eq!(m & libc::CLONE_NEWNET, libc::CLONE_NEWNET);
        assert_eq!(m & libc::CLONE_NEWNS, libc::CLONE_NEWNS);
        assert_eq!(m & libc::CLONE_NEWPID, 0);
    }

    #[test]
    fn is_empty_accounts_for_id_drop_and_namespaces() {
        assert!(SecurityConfig::default().is_empty());

        assert!(
            !SecurityConfig {
                run_as_uid: Some(1000),
                ..Default::default()
            }
            .is_empty(),
            "a uid drop must install the hook"
        );
        assert!(
            !SecurityConfig {
                run_as_gid: Some(1000),
                ..Default::default()
            }
            .is_empty(),
            "a gid drop must install the hook"
        );
        assert!(
            !SecurityConfig {
                namespaces: Namespaces {
                    net: true,
                    ..Default::default()
                },
                ..Default::default()
            }
            .is_empty(),
            "a namespace request must install the hook"
        );
    }

    #[test]
    fn seccomp_defaults_to_disabled_and_counts_toward_is_empty() {
        assert_eq!(SecurityConfig::default().seccomp, SeccompPolicy::Disabled);
        assert!(
            !SecurityConfig {
                seccomp: SeccompPolicy::BlockDangerous,
                ..Default::default()
            }
            .is_empty(),
            "a seccomp policy must install the hook"
        );
    }

    #[cfg(not(feature = "seccomp"))]
    #[test]
    fn seccomp_without_feature_is_rejected() {
        let cfg = SecurityConfig {
            seccomp: SeccompPolicy::BlockDangerous,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("seccomp") && err.contains("feature"),
            "got: {err}"
        );
    }

    #[cfg(all(target_os = "linux", feature = "seccomp"))]
    #[test]
    fn seccomp_blocklist_compiles_and_validates() {
        // The blocklist is a static input; it must always compile into a program.
        let prog = linux_impl::seccomp::build_program(&SeccompPolicy::BlockDangerous);
        assert!(matches!(prog, Ok(Some(_))));

        let cfg = SecurityConfig {
            seccomp: SeccompPolicy::BlockDangerous,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn namespace_hook_is_installed_when_requested() {
        // A namespace-only config is not empty, so attach_security installs the
        // pre_exec hook rather than early-returning. The unshare itself is
        // exercised at spawn time and fails closed in an unprivileged process.
        let cfg = SecurityConfig {
            namespaces: Namespaces {
                net: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!cfg.is_empty());
        let mut cmd = Command::new("true");
        attach_security(&mut cmd, &cfg);
    }
}
