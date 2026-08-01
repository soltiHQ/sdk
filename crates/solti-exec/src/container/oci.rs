use oci_spec::runtime::{
    Capabilities, Capability, LinuxCapabilitiesBuilder, LinuxPids, LinuxResources,
    LinuxSeccompAction, LinuxSeccompBuilder, LinuxSyscallBuilder, PosixRlimit, PosixRlimitBuilder,
    PosixRlimitType, Spec,
};
use thiserror::Error;

use super::ContainerProcessPolicy;
use crate::isolation::{LinuxCapability, SeccompPolicy, deny_host_control_syscalls};

const LINUX_EPERM: u32 = 1;

#[derive(Debug, Error)]
pub(crate) enum OciPolicyError {
    #[error("invalid container process policy: {0}")]
    InvalidPolicy(String),
    #[error("container process policy requires an OCI process")]
    MissingProcess,
    #[error("container resources or seccomp require an OCI Linux configuration")]
    MissingLinux,
    #[error("{field} exceeds the OCI signed 64-bit range")]
    OutOfRange { field: &'static str },
    #[error("cannot build OCI process policy: {0}")]
    Build(#[from] oci_spec::OciSpecError),
}

pub(crate) fn apply_process_policy(
    spec: &mut Spec,
    policy: &ContainerProcessPolicy,
) -> Result<(), OciPolicyError> {
    policy.validate().map_err(OciPolicyError::InvalidPolicy)?;

    let process_patch = policy.rlimits().is_some_and(|limits| !limits.is_empty())
        || policy.credentials().is_some()
        || policy.capabilities().is_some()
        || policy.effective_no_new_privileges()
        || policy.umask().is_some();
    let linux_patch = policy.resources().is_some() || policy.seccomp() != &SeccompPolicy::Disabled;

    if !process_patch && !linux_patch {
        return Ok(());
    }

    let mut process = if process_patch {
        Some(
            spec.process()
                .clone()
                .ok_or(OciPolicyError::MissingProcess)?,
        )
    } else {
        None
    };
    let mut linux = if linux_patch {
        Some(spec.linux().clone().ok_or(OciPolicyError::MissingLinux)?)
    } else {
        None
    };

    if let Some(process) = process.as_mut() {
        if let Some(limits) = policy.rlimits().filter(|limits| !limits.is_empty()) {
            let merged = merge_rlimits(process.rlimits().clone().unwrap_or_default(), limits)?;
            process.set_rlimits(Some(merged));
        }

        if policy.credentials().is_some() || policy.umask().is_some() {
            let mut user = process.user().clone();
            if let Some(credentials) = policy.credentials() {
                user.set_uid(credentials.uid);
                user.set_gid(credentials.gid);
                user.set_additional_gids(Some(credentials.supplementary_groups.clone()));
                user.set_username(None);
            }
            if let Some(umask) = policy.umask() {
                user.set_umask(Some(umask));
            }
            process.set_user(user);
        }

        if let Some(capabilities) = policy.capabilities() {
            let capabilities = capabilities
                .iter()
                .copied()
                .map(to_oci_capability)
                .collect::<Capabilities>();
            let replacement = LinuxCapabilitiesBuilder::default()
                .bounding(capabilities.clone())
                .effective(capabilities.clone())
                .inheritable(capabilities.clone())
                .permitted(capabilities.clone())
                .ambient(capabilities)
                .build()?;
            process.set_capabilities(Some(replacement));
        }

        if policy.effective_no_new_privileges() {
            process.set_no_new_privileges(Some(true));
        }
    }

    if let Some(linux) = linux.as_mut() {
        if let Some(limits) = policy.resources() {
            let resources = merge_resources(linux.resources().clone().unwrap_or_default(), limits)?;
            linux.set_resources(Some(resources));
        }
        if policy.seccomp() == &SeccompPolicy::DenyHostControl {
            linux.set_seccomp(Some(build_seccomp()?));
        }
    }

    if let Some(process) = process {
        spec.set_process(Some(process));
    }
    if let Some(linux) = linux {
        spec.set_linux(Some(linux));
    }
    Ok(())
}

fn merge_rlimits(
    mut current: Vec<PosixRlimit>,
    limits: &crate::isolation::RlimitConfig,
) -> Result<Vec<PosixRlimit>, oci_spec::OciSpecError> {
    if limits.max_open_files.is_some() {
        current.retain(|limit| limit.typ() != PosixRlimitType::RlimitNofile);
    }
    if limits.max_file_size_bytes.is_some() {
        current.retain(|limit| limit.typ() != PosixRlimitType::RlimitFsize);
    }
    if limits.disable_core_dumps {
        current.retain(|limit| limit.typ() != PosixRlimitType::RlimitCore);
    }

    if let Some(value) = limits.max_open_files {
        current.push(rlimit(PosixRlimitType::RlimitNofile, value)?);
    }
    if let Some(value) = limits.max_file_size_bytes {
        current.push(rlimit(PosixRlimitType::RlimitFsize, value)?);
    }
    if limits.disable_core_dumps {
        current.push(rlimit(PosixRlimitType::RlimitCore, 0)?);
    }
    Ok(current)
}

fn rlimit(typ: PosixRlimitType, value: u64) -> Result<PosixRlimit, oci_spec::OciSpecError> {
    PosixRlimitBuilder::default()
        .typ(typ)
        .hard(value)
        .soft(value)
        .build()
}

fn merge_resources(
    mut current: LinuxResources,
    limits: &crate::isolation::CgroupLimits,
) -> Result<LinuxResources, OciPolicyError> {
    if let Some(limit) = limits.cpu {
        let mut cpu = current.cpu().clone().unwrap_or_default();
        cpu.set_quota(
            limit
                .quota
                .map(|value| signed(value, "container.resources.cpu.quota"))
                .transpose()?,
        );
        cpu.set_period(Some(limit.period));
        current.set_cpu(Some(cpu));
    }
    if let Some(limit) = limits.memory {
        let mut memory = current.memory().as_ref().copied().unwrap_or_default();
        memory.set_limit(Some(signed(limit, "container.resources.memory")?));
        current.set_memory(Some(memory));
    }
    if let Some(limit) = limits.pids {
        let mut pids = LinuxPids::default();
        pids.set_limit(signed(limit, "container.resources.pids")?);
        current.set_pids(Some(pids));
    }
    Ok(current)
}

fn signed(value: u64, field: &'static str) -> Result<i64, OciPolicyError> {
    i64::try_from(value).map_err(|_| OciPolicyError::OutOfRange { field })
}

fn build_seccomp() -> Result<oci_spec::runtime::LinuxSeccomp, oci_spec::OciSpecError> {
    let names = deny_host_control_syscalls()
        .iter()
        .map(|syscall| syscall.name().to_owned())
        .collect::<Vec<_>>();
    let deny = LinuxSyscallBuilder::default()
        .names(names)
        .action(LinuxSeccompAction::ScmpActErrno)
        .errno_ret(LINUX_EPERM)
        .build()?;

    LinuxSeccompBuilder::default()
        .default_action(LinuxSeccompAction::ScmpActAllow)
        .syscalls(vec![deny])
        .build()
}

fn to_oci_capability(capability: LinuxCapability) -> Capability {
    match capability {
        LinuxCapability::Chown => Capability::Chown,
        LinuxCapability::DacOverride => Capability::DacOverride,
        LinuxCapability::DacReadSearch => Capability::DacReadSearch,
        LinuxCapability::FOwner => Capability::Fowner,
        LinuxCapability::FSetId => Capability::Fsetid,
        LinuxCapability::Kill => Capability::Kill,
        LinuxCapability::SetGid => Capability::Setgid,
        LinuxCapability::SetUid => Capability::Setuid,
        LinuxCapability::SetPCap => Capability::Setpcap,
        LinuxCapability::NetBindService => Capability::NetBindService,
        LinuxCapability::NetRaw => Capability::NetRaw,
        LinuxCapability::NetAdmin => Capability::NetAdmin,
        LinuxCapability::SysChroot => Capability::SysChroot,
        LinuxCapability::SysPtrace => Capability::SysPtrace,
        LinuxCapability::SysAdmin => Capability::SysAdmin,
        LinuxCapability::SysBoot => Capability::SysBoot,
        LinuxCapability::SysNice => Capability::SysNice,
        LinuxCapability::SysResource => Capability::SysResource,
        LinuxCapability::SysTime => Capability::SysTime,
        LinuxCapability::MkNod => Capability::Mknod,
        LinuxCapability::AuditWrite => Capability::AuditWrite,
        LinuxCapability::AuditControl => Capability::AuditControl,
        LinuxCapability::SetFCap => Capability::Setfcap,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::isolation::{CgroupLimits, CpuMax, ProcessCredentials, RlimitConfig};

    #[test]
    fn empty_policy_preserves_the_complete_spec() {
        let mut spec = Spec::default();
        let original = spec.clone();

        apply_process_policy(&mut spec, &ContainerProcessPolicy::new()).unwrap();

        assert_eq!(spec, original);
    }

    #[test]
    fn rlimits_replace_selected_types_and_preserve_unrelated_entries() {
        let stack = rlimit(PosixRlimitType::RlimitStack, 4096).unwrap();
        let old_nofile = rlimit(PosixRlimitType::RlimitNofile, 64).unwrap();
        let process = Spec::default().process().clone().unwrap();
        let mut process = process;
        process.set_rlimits(Some(vec![stack, old_nofile]));
        let mut spec = Spec::default();
        spec.set_process(Some(process));
        let policy = ContainerProcessPolicy::new().with_rlimits(RlimitConfig {
            max_open_files: Some(1024),
            max_file_size_bytes: Some(8192),
            disable_core_dumps: true,
        });

        apply_process_policy(&mut spec, &policy).unwrap();

        let limits = spec.process().as_ref().unwrap().rlimits().as_ref().unwrap();
        assert_eq!(limits[0].typ(), PosixRlimitType::RlimitStack);
        assert_eq!(limits[1].typ(), PosixRlimitType::RlimitNofile);
        assert_eq!(limits[1].hard(), 1024);
        assert_eq!(limits[2].typ(), PosixRlimitType::RlimitFsize);
        assert_eq!(limits[2].hard(), 8192);
        assert_eq!(limits[3].typ(), PosixRlimitType::RlimitCore);
        assert_eq!(limits[3].hard(), 0);
    }

    #[test]
    fn credentials_are_exact_and_preserve_an_existing_umask() {
        let mut spec = Spec::default();
        let process = spec.process_mut().as_mut().unwrap();
        process.user_mut().set_username(Some("image-user".into()));
        process.user_mut().set_umask(Some(0o027));
        let policy = ContainerProcessPolicy::new()
            .with_credentials(ProcessCredentials::new(1000, 1001).with_supplementary_groups([]))
            .with_no_new_privileges(true);

        apply_process_policy(&mut spec, &policy).unwrap();

        let user = spec.process().as_ref().unwrap().user();
        assert_eq!(user.uid(), 1000);
        assert_eq!(user.gid(), 1001);
        assert_eq!(user.additional_gids(), &Some(Vec::new()));
        assert_eq!(user.username(), &None);
        assert_eq!(user.umask(), Some(0o027));
    }

    #[test]
    fn resources_replace_only_the_selected_nodes() {
        let mut spec = Spec::default();
        let policy = ContainerProcessPolicy::new().with_resources(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(50_000),
                period: 100_000,
            }),
            memory: Some(64 * 1024 * 1024),
            pids: Some(32),
        });

        apply_process_policy(&mut spec, &policy).unwrap();

        let resources = spec.linux().as_ref().unwrap().resources().as_ref().unwrap();
        assert_eq!(resources.cpu().as_ref().unwrap().quota(), Some(50_000));
        assert_eq!(resources.cpu().as_ref().unwrap().period(), Some(100_000));
        assert_eq!(
            resources.memory().as_ref().unwrap().limit(),
            Some(64 * 1024 * 1024)
        );
        assert_eq!(resources.pids().as_ref().unwrap().limit(), 32);
    }

    #[test]
    fn signed_oci_resource_overflow_is_rejected_before_mutation() {
        let mut spec = Spec::default();
        let original = spec.clone();
        let policy = ContainerProcessPolicy::new().with_resources(CgroupLimits {
            memory: Some(u64::MAX),
            ..Default::default()
        });

        let error = apply_process_policy(&mut spec, &policy).unwrap_err();

        assert!(matches!(
            error,
            OciPolicyError::OutOfRange {
                field: "container.resources.memory"
            }
        ));
        assert_eq!(spec, original);
    }

    #[test]
    fn capability_replacement_updates_all_five_sets_and_deduplicates() {
        let mut spec = Spec::default();
        let policy = ContainerProcessPolicy::new()
            .with_capabilities([
                LinuxCapability::NetBindService,
                LinuxCapability::NetBindService,
            ])
            .with_no_new_privileges(true);

        apply_process_policy(&mut spec, &policy).unwrap();

        let capabilities = spec
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .as_ref()
            .unwrap();
        let expected = HashSet::from([Capability::NetBindService]);
        assert_eq!(capabilities.bounding(), &Some(expected.clone()));
        assert_eq!(capabilities.effective(), &Some(expected.clone()));
        assert_eq!(capabilities.inheritable(), &Some(expected.clone()));
        assert_eq!(capabilities.permitted(), &Some(expected.clone()));
        assert_eq!(capabilities.ambient(), &Some(expected));
    }

    #[test]
    fn seccomp_uses_the_named_denylist_and_enables_no_new_privileges() {
        let mut spec = Spec::default();
        spec.process_mut()
            .as_mut()
            .unwrap()
            .set_no_new_privileges(Some(false));
        let policy = ContainerProcessPolicy::new().with_seccomp(SeccompPolicy::DenyHostControl);

        apply_process_policy(&mut spec, &policy).unwrap();

        assert_eq!(
            spec.process().as_ref().unwrap().no_new_privileges(),
            Some(true)
        );
        let seccomp = spec.linux().as_ref().unwrap().seccomp().as_ref().unwrap();
        assert_eq!(seccomp.default_action(), LinuxSeccompAction::ScmpActAllow);
        let rules = seccomp.syscalls().as_ref().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].action(), LinuxSeccompAction::ScmpActErrno);
        assert_eq!(rules[0].errno_ret(), Some(LINUX_EPERM));
        assert!(rules[0].names().iter().any(|name| name == "mount"));
        assert!(rules[0].names().iter().any(|name| name == "ptrace"));
    }

    #[test]
    fn explicit_empty_capabilities_drop_every_set() {
        let mut spec = Spec::default();
        let policy = ContainerProcessPolicy::new()
            .with_capabilities([])
            .with_no_new_privileges(true);

        apply_process_policy(&mut spec, &policy).unwrap();

        let capabilities = spec
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .as_ref()
            .unwrap();
        assert!(capabilities.bounding().as_ref().unwrap().is_empty());
        assert!(capabilities.effective().as_ref().unwrap().is_empty());
        assert!(capabilities.inheritable().as_ref().unwrap().is_empty());
        assert!(capabilities.permitted().as_ref().unwrap().is_empty());
        assert!(capabilities.ambient().as_ref().unwrap().is_empty());
    }
}
