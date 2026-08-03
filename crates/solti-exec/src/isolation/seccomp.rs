//! # Seccomp policy

/// Seccomp policy applied to a Linux process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SeccompPolicy {
    /// Does not install a syscall filter.
    #[default]
    Disabled,
    /// Rejects a fixed host-control syscall denylist with `EPERM`.
    ///
    /// The default action remains `Allow`.
    /// This policy is not a syscall sandbox.
    /// It enables `no_new_privileges` when applied.
    DenyHostControl,
}

#[cfg(any(feature = "containerd", all(feature = "seccomp", target_os = "linux")))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeniedSyscall {
    #[cfg(feature = "containerd")]
    name: &'static str,
    #[cfg(all(feature = "seccomp", target_os = "linux"))]
    number: libc::c_long,
}

#[cfg(any(feature = "containerd", all(feature = "seccomp", target_os = "linux")))]
impl DeniedSyscall {
    #[cfg(feature = "containerd")]
    pub(crate) const fn name(self) -> &'static str {
        self.name
    }

    #[cfg(all(feature = "seccomp", target_os = "linux"))]
    pub(crate) const fn number(self) -> libc::c_long {
        self.number
    }
}

#[cfg(any(feature = "containerd", all(feature = "seccomp", target_os = "linux")))]
macro_rules! define_deny_host_control_syscalls {
    ($( $(#[$attribute:meta])* $name:literal => $number:path ),+ $(,)?) => {
        pub(crate) fn deny_host_control_syscalls() -> &'static [DeniedSyscall] {
            &[
                $(
                    $(#[$attribute])*
                    DeniedSyscall {
                        #[cfg(feature = "containerd")]
                        name: $name,
                        #[cfg(all(feature = "seccomp", target_os = "linux"))]
                        number: $number,
                    },
                )+
            ]
        }
    };
}

#[cfg(any(feature = "containerd", all(feature = "seccomp", target_os = "linux")))]
define_deny_host_control_syscalls! {
    "ptrace" => libc::SYS_ptrace,
    "process_vm_readv" => libc::SYS_process_vm_readv,
    "process_vm_writev" => libc::SYS_process_vm_writev,
    "process_madvise" => libc::SYS_process_madvise,
    "pidfd_getfd" => libc::SYS_pidfd_getfd,
    "kcmp" => libc::SYS_kcmp,
    "mount" => libc::SYS_mount,
    "umount2" => libc::SYS_umount2,
    "pivot_root" => libc::SYS_pivot_root,
    "open_tree" => libc::SYS_open_tree,
    "move_mount" => libc::SYS_move_mount,
    "fsopen" => libc::SYS_fsopen,
    "fsconfig" => libc::SYS_fsconfig,
    "fsmount" => libc::SYS_fsmount,
    "fspick" => libc::SYS_fspick,
    "mount_setattr" => libc::SYS_mount_setattr,
    "name_to_handle_at" => libc::SYS_name_to_handle_at,
    "open_by_handle_at" => libc::SYS_open_by_handle_at,
    "quotactl" => libc::SYS_quotactl,
    "quotactl_fd" => libc::SYS_quotactl_fd,
    "kexec_load" => libc::SYS_kexec_load,
    "kexec_file_load" => libc::SYS_kexec_file_load,
    "init_module" => libc::SYS_init_module,
    "finit_module" => libc::SYS_finit_module,
    "delete_module" => libc::SYS_delete_module,
    "bpf" => libc::SYS_bpf,
    "perf_event_open" => libc::SYS_perf_event_open,
    "swapon" => libc::SYS_swapon,
    "swapoff" => libc::SYS_swapoff,
    "reboot" => libc::SYS_reboot,
    "sethostname" => libc::SYS_sethostname,
    "setdomainname" => libc::SYS_setdomainname,
    "settimeofday" => libc::SYS_settimeofday,
    "clock_settime" => libc::SYS_clock_settime,
    "adjtimex" => libc::SYS_adjtimex,
    "clock_adjtime" => libc::SYS_clock_adjtime,
    "setns" => libc::SYS_setns,
    "acct" => libc::SYS_acct,
    "syslog" => libc::SYS_syslog,
    "fanotify_init" => libc::SYS_fanotify_init,
    "lookup_dcookie" => libc::SYS_lookup_dcookie,
    "vhangup" => libc::SYS_vhangup,
    "add_key" => libc::SYS_add_key,
    "keyctl" => libc::SYS_keyctl,
    "request_key" => libc::SYS_request_key,
    #[cfg(target_arch = "x86_64")]
    "iopl" => libc::SYS_iopl,
    #[cfg(target_arch = "x86_64")]
    "ioperm" => libc::SYS_ioperm,
}
