//! OCI runtime specification rendering for containerd.

use std::path::PathBuf;

use oci_spec::{
    image::ImageConfiguration,
    runtime::{
        Capabilities, Capability, LinuxBuilder, LinuxCapabilitiesBuilder, LinuxDeviceCgroupBuilder,
        LinuxDeviceType, LinuxNamespaceBuilder, LinuxNamespaceType, LinuxResourcesBuilder, Mount,
        MountBuilder, PosixRlimitBuilder, PosixRlimitType, ProcessBuilder, RootBuilder, Spec,
        SpecBuilder, UserBuilder, VERSION,
    },
};

use super::{
    ContainerNetwork, ContainerdConfig, config::validate_identifier, image::ResolvedImage,
};
use crate::container::{ContainerEngineError, ContainerRequest};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const MASKED_PATHS: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
    "/proc/scsi",
];

const READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// Builds the Linux OCI specification stored in a containerd container record.
pub(super) fn build(
    request: &ContainerRequest,
    resolved: &ResolvedImage,
    config: &ContainerdConfig,
    resource_id: &str,
) -> Result<Spec, ContainerEngineError> {
    if !config.platform().os().eq_ignore_ascii_case("linux") {
        return Err(ContainerEngineError::permanent(format!(
            "native containerd execution supports Linux images only, got {:?}",
            config.platform().os()
        )));
    }
    build_configuration(
        request,
        &resolved.configuration,
        config.namespace(),
        config.network(),
        resource_id,
    )
}

fn build_configuration(
    request: &ContainerRequest,
    image: &ImageConfiguration,
    namespace: &str,
    network: ContainerNetwork,
    resource_id: &str,
) -> Result<Spec, ContainerEngineError> {
    validate_identifier("namespace", namespace)?;
    validate_identifier("resource ID", resource_id)?;
    validate_runtime_credentials(request)?;

    let image_config = image.config().as_ref();
    let args = resolve_args(request, image_config)?;
    let env = resolve_env(request, image_config)?;
    let cwd = resolve_cwd(image_config)?;
    let user = resolve_user(request, image_config)?;

    let capabilities = default_capabilities();
    let mut process_capabilities = LinuxCapabilitiesBuilder::default()
        .bounding(capabilities.clone())
        .effective(capabilities.clone())
        .permitted(capabilities)
        .build()
        .map_err(oci_build_error)?;
    process_capabilities.set_inheritable(None);
    process_capabilities.set_ambient(None);

    let process = ProcessBuilder::default()
        .terminal(false)
        .user(user)
        .args(args)
        .env(env)
        .cwd(cwd)
        .capabilities(process_capabilities)
        .rlimits(vec![
            PosixRlimitBuilder::default()
                .typ(PosixRlimitType::RlimitNofile)
                .hard(1024_u64)
                .soft(1024_u64)
                .build()
                .map_err(oci_build_error)?,
        ])
        .no_new_privileges(true)
        .build()
        .map_err(oci_build_error)?;

    let mut namespaces = vec![
        namespace_entry(LinuxNamespaceType::Pid)?,
        namespace_entry(LinuxNamespaceType::Ipc)?,
        namespace_entry(LinuxNamespaceType::Uts)?,
        namespace_entry(LinuxNamespaceType::Mount)?,
    ];
    if network == ContainerNetwork::None {
        namespaces.push(namespace_entry(LinuxNamespaceType::Network)?);
    }

    let resources = LinuxResourcesBuilder::default()
        .devices(default_device_rules()?)
        .build()
        .map_err(oci_build_error)?;
    let linux = LinuxBuilder::default()
        .resources(resources)
        .cgroups_path(PathBuf::from(format!("/{namespace}/{resource_id}")))
        .namespaces(namespaces)
        .masked_paths(strings(MASKED_PATHS))
        .readonly_paths(strings(READONLY_PATHS))
        .build()
        .map_err(oci_build_error)?;

    let mut spec = SpecBuilder::default()
        .version(VERSION)
        .root(
            RootBuilder::default()
                .path("rootfs")
                .readonly(false)
                .build()
                .map_err(oci_build_error)?,
        )
        .mounts(default_mounts()?)
        .process(process)
        .linux(linux)
        .build()
        .map_err(oci_build_error)?;
    spec.set_hostname(None);
    spec.set_annotations(None);

    super::super::oci::apply_process_policy(&mut spec, request.process_policy()).map_err(
        |error| {
            ContainerEngineError::permanent_from(
                "cannot apply container process policy to OCI specification",
                error,
            )
        },
    )?;
    Ok(spec)
}

fn resolve_args(
    request: &ContainerRequest,
    image: Option<&oci_spec::image::Config>,
) -> Result<Vec<String>, ContainerEngineError> {
    let entrypoint = request
        .command()
        .map(<[String]>::to_vec)
        .or_else(|| image.and_then(|config| config.entrypoint().clone()))
        .unwrap_or_default();
    let command = if request.args().is_empty() {
        image
            .and_then(|config| config.cmd().clone())
            .unwrap_or_default()
    } else {
        request.args().to_vec()
    };

    let args = entrypoint.into_iter().chain(command).collect::<Vec<_>>();
    if args.is_empty() {
        return Err(ContainerEngineError::permanent(
            "container image and task define no executable",
        ));
    }
    if args.iter().any(|value| value.contains('\0')) {
        return Err(ContainerEngineError::permanent(
            "container image command contains NUL",
        ));
    }
    Ok(args)
}

fn resolve_env(
    request: &ContainerRequest,
    image: Option<&oci_spec::image::Config>,
) -> Result<Vec<String>, ContainerEngineError> {
    let image_env = image.and_then(|config| config.env().as_deref());
    let mut merged = Vec::<(String, String)>::new();

    match image_env {
        Some(entries) if !entries.is_empty() => {
            for entry in entries {
                let (name, value) = parse_env(entry)?;
                if let Some((_, current)) = merged.iter_mut().find(|(key, _)| key == name) {
                    *current = value.to_owned();
                } else {
                    merged.push((name.to_owned(), value.to_owned()));
                }
            }
        }
        _ => merged.push(("PATH".to_owned(), DEFAULT_PATH.to_owned())),
    }

    for (name, value) in request.env() {
        validate_env(name, value)?;
        if let Some((_, current)) = merged.iter_mut().find(|(key, _)| key == name) {
            *current = value.clone();
        } else {
            merged.push((name.clone(), value.clone()));
        }
    }

    Ok(merged
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect())
}

fn parse_env(entry: &str) -> Result<(&str, &str), ContainerEngineError> {
    let (name, value) = entry.split_once('=').ok_or_else(|| {
        ContainerEngineError::permanent(
            "invalid container image environment entry: expected NAME=VALUE",
        )
    })?;
    validate_env(name, value)?;
    Ok((name, value))
}

fn validate_env(name: &str, value: &str) -> Result<(), ContainerEngineError> {
    if name.is_empty() || name.contains(['=', '\0']) {
        return Err(ContainerEngineError::permanent(format!(
            "invalid container environment variable name {name:?}"
        )));
    }
    if value.contains('\0') {
        return Err(ContainerEngineError::permanent(format!(
            "container environment variable {name:?} contains NUL"
        )));
    }
    Ok(())
}

fn resolve_cwd(image: Option<&oci_spec::image::Config>) -> Result<PathBuf, ContainerEngineError> {
    let cwd = image
        .and_then(|config| config.working_dir().as_deref())
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or("/");
    if cwd.contains('\0') || !cwd.starts_with('/') {
        return Err(ContainerEngineError::permanent(
            "container image working directory must be an absolute path without NUL",
        ));
    }
    Ok(PathBuf::from(cwd))
}

fn resolve_user(
    request: &ContainerRequest,
    image: Option<&oci_spec::image::Config>,
) -> Result<oci_spec::runtime::User, ContainerEngineError> {
    if request.process_policy().credentials().is_some() {
        return UserBuilder::default().build().map_err(oci_build_error);
    }

    let user = image
        .and_then(|config| config.user().as_deref())
        .filter(|user| !user.is_empty());
    let Some(user) = user else {
        return UserBuilder::default().build().map_err(oci_build_error);
    };
    let Some((uid, gid)) = user.split_once(':') else {
        return Err(unsupported_image_user(user));
    };
    if uid.is_empty() || gid.is_empty() || gid.contains(':') {
        return Err(unsupported_image_user(user));
    }
    let uid = uid
        .parse::<u32>()
        .map_err(|_| unsupported_image_user(user))?;
    let gid = gid
        .parse::<u32>()
        .map_err(|_| unsupported_image_user(user))?;
    if uid > i32::MAX as u32 || gid > i32::MAX as u32 {
        return Err(ContainerEngineError::permanent(
            "container image user exceeds the containerd runtime ID range",
        ));
    }

    UserBuilder::default()
        .uid(uid)
        .gid(gid)
        .build()
        .map_err(oci_build_error)
}

fn unsupported_image_user(_user: &str) -> ContainerEngineError {
    ContainerEngineError::permanent(
        "container image user requires filesystem identity resolution; configure exact numeric container credentials or use UID:GID",
    )
}

fn validate_runtime_credentials(request: &ContainerRequest) -> Result<(), ContainerEngineError> {
    let Some(credentials) = request.process_policy().credentials() else {
        return Ok(());
    };
    if [credentials.uid, credentials.gid]
        .into_iter()
        .chain(credentials.supplementary_groups.iter().copied())
        .any(|id| id > i32::MAX as u32)
    {
        return Err(ContainerEngineError::permanent(
            "container credentials exceed the containerd runtime ID range",
        ));
    }
    Ok(())
}

fn default_capabilities() -> Capabilities {
    [
        Capability::Chown,
        Capability::DacOverride,
        Capability::Fsetid,
        Capability::Fowner,
        Capability::Mknod,
        Capability::NetRaw,
        Capability::Setgid,
        Capability::Setuid,
        Capability::Setfcap,
        Capability::Setpcap,
        Capability::NetBindService,
        Capability::SysChroot,
        Capability::Kill,
        Capability::AuditWrite,
    ]
    .into_iter()
    .collect()
}

fn default_device_rules() -> Result<Vec<oci_spec::runtime::LinuxDeviceCgroup>, ContainerEngineError>
{
    let mut rules = vec![
        LinuxDeviceCgroupBuilder::default()
            .allow(false)
            .access("rwm")
            .build()
            .map_err(oci_build_error)?,
    ];
    for (major, minor) in [
        (1, Some(3)),
        (1, Some(8)),
        (1, Some(7)),
        (5, Some(0)),
        (1, Some(5)),
        (1, Some(9)),
        (5, Some(1)),
        (136, None),
        (5, Some(2)),
    ] {
        let mut rule = LinuxDeviceCgroupBuilder::default()
            .allow(true)
            .typ(LinuxDeviceType::C)
            .major(major)
            .access("rwm");
        if let Some(minor) = minor {
            rule = rule.minor(minor);
        }
        rules.push(rule.build().map_err(oci_build_error)?);
    }
    Ok(rules)
}

fn namespace_entry(
    typ: LinuxNamespaceType,
) -> Result<oci_spec::runtime::LinuxNamespace, ContainerEngineError> {
    LinuxNamespaceBuilder::default()
        .typ(typ)
        .build()
        .map_err(oci_build_error)
}

fn default_mounts() -> Result<Vec<Mount>, ContainerEngineError> {
    [
        ("/proc", "proc", "proc", &["nosuid", "noexec", "nodev"][..]),
        (
            "/dev",
            "tmpfs",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"][..],
        ),
        (
            "/dev/pts",
            "devpts",
            "devpts",
            &[
                "nosuid",
                "noexec",
                "newinstance",
                "ptmxmode=0666",
                "mode=0620",
                "gid=5",
            ][..],
        ),
        (
            "/dev/shm",
            "tmpfs",
            "shm",
            &["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"][..],
        ),
        (
            "/dev/mqueue",
            "mqueue",
            "mqueue",
            &["nosuid", "noexec", "nodev"][..],
        ),
        (
            "/sys",
            "sysfs",
            "sysfs",
            &["nosuid", "noexec", "nodev", "ro"][..],
        ),
        (
            "/run",
            "tmpfs",
            "tmpfs",
            &["nosuid", "strictatime", "mode=755", "size=65536k"][..],
        ),
    ]
    .into_iter()
    .map(|(destination, typ, source, options)| {
        MountBuilder::default()
            .destination(destination)
            .typ(typ)
            .source(source)
            .options(strings(options))
            .build()
            .map_err(oci_build_error)
    })
    .collect()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn oci_build_error(error: oci_spec::OciSpecError) -> ContainerEngineError {
    ContainerEngineError::permanent_from("cannot build OCI runtime specification", error)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use oci_spec::runtime::{Capability, LinuxNamespaceType, PosixRlimitType};
    use serde_json::json;

    use super::*;
    use crate::{
        container::ContainerProcessPolicy,
        isolation::{LinuxCapability, ProcessCredentials},
    };

    fn image(config: serde_json::Value) -> ImageConfiguration {
        serde_json::from_value(json!({
            "architecture": "amd64",
            "os": "linux",
            "config": config,
            "rootfs": { "type": "layers", "diff_ids": [] }
        }))
        .unwrap()
    }

    fn request(
        command: Option<&[&str]>,
        args: &[&str],
        env: &[(&str, &str)],
        policy: ContainerProcessPolicy,
    ) -> ContainerRequest {
        ContainerRequest {
            attempt_id: "container-slot-1-a1".into(),
            task_name: solti_model::TaskId::new("task").unwrap(),
            generation: 1,
            attempt: 1,
            image: "example.test/image:latest".into(),
            command: command.map(|values| values.iter().map(|value| (*value).into()).collect()),
            args: args.iter().map(|value| (*value).into()).collect(),
            env: env
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect::<BTreeMap<_, _>>(),
            process_policy: policy,
        }
    }

    fn args(spec: &Spec) -> &[String] {
        spec.process().as_ref().unwrap().args().as_deref().unwrap()
    }

    fn render(
        request: &ContainerRequest,
        image: &ImageConfiguration,
        network: ContainerNetwork,
    ) -> Result<Spec, ContainerEngineError> {
        build_configuration(request, image, "solti", network, request.attempt_id())
    }

    #[test]
    fn command_and_args_follow_kubernetes_override_rules() {
        let image = image(json!({
            "Entrypoint": ["/image-entry"],
            "Cmd": ["image-arg"]
        }));

        let cases = [
            (None, &[][..], vec!["/image-entry", "image-arg"]),
            (None, &["task-arg"][..], vec!["/image-entry", "task-arg"]),
            (
                Some(&["/task-entry"][..]),
                &[][..],
                vec!["/task-entry", "image-arg"],
            ),
            (
                Some(&["/task-entry"][..]),
                &["task-arg"][..],
                vec!["/task-entry", "task-arg"],
            ),
        ];

        for (command, task_args, expected) in cases {
            let request = request(command, task_args, &[], ContainerProcessPolicy::new());
            let spec = render(&request, &image, ContainerNetwork::None).unwrap();
            assert_eq!(args(&spec), expected);
        }
    }

    #[test]
    fn empty_command_is_rejected() {
        let image = image(json!({}));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let error = render(&request, &image, ContainerNetwork::None).unwrap_err();

        assert_eq!(
            error.reason(),
            "container image and task define no executable"
        );
    }

    #[test]
    fn task_environment_overrides_image_values_and_preserves_default_order() {
        let image = image(json!({
            "Entrypoint": ["/bin/app"],
            "Env": ["PATH=/image/bin", "MODE=image", "EMPTY="]
        }));
        let request = request(
            None,
            &[],
            &[("MODE", "task"), ("NEW", "value")],
            ContainerProcessPolicy::new(),
        );

        let spec = render(&request, &image, ContainerNetwork::None).unwrap();
        let env = spec.process().as_ref().unwrap().env().as_ref().unwrap();

        assert_eq!(
            env,
            &["PATH=/image/bin", "MODE=task", "EMPTY=", "NEW=value"]
        );
    }

    #[test]
    fn empty_image_environment_gets_containerd_safe_path() {
        let image = image(json!({ "Entrypoint": ["app"], "Env": [] }));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let spec = render(&request, &image, ContainerNetwork::None).unwrap();

        assert_eq!(
            spec.process().as_ref().unwrap().env().as_ref().unwrap(),
            &[format!("PATH={DEFAULT_PATH}")]
        );
    }

    #[test]
    fn malformed_image_environment_is_rejected() {
        const SECRET: &str = "malformed-image-environment-secret";
        let image = image(json!({ "Entrypoint": ["app"], "Env": [SECRET] }));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let error = render(&request, &image, ContainerNetwork::None).unwrap_err();

        assert_eq!(
            error.reason(),
            "invalid container image environment entry: expected NAME=VALUE"
        );
        assert!(!error.reason().contains(SECRET));
        assert!(!format!("{error}").contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn invalid_image_working_directory_is_redacted() {
        const SECRET: &str = "relative-image-working-directory-secret";
        let image = image(json!({ "Entrypoint": ["app"], "WorkingDir": SECRET }));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let error = render(&request, &image, ContainerNetwork::None).unwrap_err();

        assert_eq!(
            error.reason(),
            "container image working directory must be an absolute path without NUL"
        );
        assert!(!error.reason().contains(SECRET));
        assert!(!format!("{error}").contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn numeric_image_user_is_applied_and_named_user_is_rejected() {
        const SECRET: &str = "named-image-user-secret";
        let numeric = image(json!({ "Entrypoint": ["app"], "User": "1000:1001" }));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());
        let spec = render(&request, &numeric, ContainerNetwork::None).unwrap();
        let user = spec.process().as_ref().unwrap().user();
        assert_eq!((user.uid(), user.gid()), (1000, 1001));

        let named = image(json!({ "Entrypoint": ["app"], "User": SECRET }));
        let error = render(&request, &named, ContainerNetwork::None).unwrap_err();
        assert!(
            error
                .reason()
                .contains("requires filesystem identity resolution")
        );
        assert!(!error.reason().contains(SECRET));
        assert!(!format!("{error}").contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
    }

    #[test]
    fn explicit_numeric_policy_overrides_named_image_user() {
        let image = image(json!({ "Entrypoint": ["app"], "User": "worker" }));
        let policy = ContainerProcessPolicy::new()
            .with_credentials(ProcessCredentials::new(2000, 2001))
            .with_no_new_privileges(true);
        let request = request(None, &[], &[], policy);

        let spec = render(&request, &image, ContainerNetwork::None).unwrap();
        let user = spec.process().as_ref().unwrap().user();

        assert_eq!((user.uid(), user.gid()), (2000, 2001));
    }

    #[test]
    fn network_mode_only_controls_the_network_namespace() {
        let image = image(json!({ "Entrypoint": ["app"] }));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let isolated = render(&request, &image, ContainerNetwork::None).unwrap();
        let host = render(&request, &image, ContainerNetwork::Host).unwrap();
        let namespace_types = |spec: &Spec| {
            spec.linux()
                .as_ref()
                .unwrap()
                .namespaces()
                .as_ref()
                .unwrap()
                .iter()
                .map(|namespace| namespace.typ())
                .collect::<HashSet<_>>()
        };

        let isolated = namespace_types(&isolated);
        let host = namespace_types(&host);
        assert!(isolated.contains(&LinuxNamespaceType::Network));
        assert!(!host.contains(&LinuxNamespaceType::Network));
        for typ in [
            LinuxNamespaceType::Pid,
            LinuxNamespaceType::Ipc,
            LinuxNamespaceType::Uts,
            LinuxNamespaceType::Mount,
        ] {
            assert!(isolated.contains(&typ));
            assert!(host.contains(&typ));
        }
        assert!(!isolated.contains(&LinuxNamespaceType::Cgroup));
        assert!(!host.contains(&LinuxNamespaceType::Cgroup));
    }

    #[test]
    fn non_linux_platform_is_rejected() {
        let image = image(json!({ "Entrypoint": ["app"] }));
        let resolved = ResolvedImage {
            reference: "example.test/image:latest".into(),
            configuration: image,
            chain_id: String::new(),
        };
        let config = ContainerdConfig::new(
            "/run/containerd/containerd.sock",
            "solti",
            "overlayfs",
            "io.containerd.runc.v2",
        )
        .with_platform(super::super::ContainerPlatform::new("windows", "amd64", ""));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let error = build(&request, &resolved, &config, request.attempt_id()).unwrap_err();

        assert_eq!(
            error.reason(),
            "native containerd execution supports Linux images only, got \"windows\""
        );
    }

    #[test]
    fn baseline_matches_containerd_security_defaults() {
        let image = image(json!({ "Entrypoint": ["app"] }));
        let request = request(None, &[], &[], ContainerProcessPolicy::new());

        let spec = render(&request, &image, ContainerNetwork::None).unwrap();
        let process = spec.process().as_ref().unwrap();
        let linux = spec.linux().as_ref().unwrap();

        assert_eq!(spec.version(), VERSION);
        assert_eq!(spec.hostname(), &None);
        assert_eq!(spec.annotations(), &None);
        assert_eq!(
            spec.root().as_ref().unwrap().path(),
            &PathBuf::from("rootfs")
        );
        assert_eq!(spec.root().as_ref().unwrap().readonly(), Some(false));
        assert_eq!(process.cwd(), &PathBuf::from("/"));
        assert_eq!(process.no_new_privileges(), Some(true));
        assert_eq!(
            process.rlimits().as_ref().unwrap()[0].typ(),
            PosixRlimitType::RlimitNofile
        );
        assert_eq!(process.rlimits().as_ref().unwrap()[0].soft(), 1024);
        assert_eq!(process.rlimits().as_ref().unwrap()[0].hard(), 1024);

        let capabilities = process.capabilities().as_ref().unwrap();
        let expected = default_capabilities();
        assert_eq!(capabilities.bounding().as_ref(), Some(&expected));
        assert_eq!(capabilities.effective().as_ref(), Some(&expected));
        assert_eq!(capabilities.permitted().as_ref(), Some(&expected));
        assert_eq!(capabilities.inheritable(), &None);
        assert_eq!(capabilities.ambient(), &None);

        let devices = linux
            .resources()
            .as_ref()
            .unwrap()
            .devices()
            .as_ref()
            .unwrap();
        assert_eq!(devices.len(), 10);
        assert!(!devices[0].allow());
        assert_eq!(devices[0].access().as_deref(), Some("rwm"));
        assert!(devices[1..].iter().all(|device| device.allow()));
        assert_eq!(
            linux.cgroups_path().as_ref().unwrap(),
            &PathBuf::from("/solti/container-slot-1-a1")
        );
        assert_eq!(
            linux.masked_paths().as_ref().unwrap(),
            &strings(MASKED_PATHS)
        );
        assert_eq!(
            linux.readonly_paths().as_ref().unwrap(),
            &strings(READONLY_PATHS)
        );

        let mount_points = spec
            .mounts()
            .as_ref()
            .unwrap()
            .iter()
            .map(|mount| mount.destination().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            mount_points,
            [
                "/proc",
                "/dev",
                "/dev/pts",
                "/dev/shm",
                "/dev/mqueue",
                "/sys",
                "/run"
            ]
        );
    }

    #[test]
    fn process_policy_is_applied_after_image_and_baseline() {
        let image = image(json!({ "Entrypoint": ["app"] }));
        let policy = ContainerProcessPolicy::new()
            .with_capabilities([LinuxCapability::NetBindService])
            .with_no_new_privileges(true);
        let request = request(None, &[], &[], policy);

        let spec = render(&request, &image, ContainerNetwork::None).unwrap();
        let capabilities = spec
            .process()
            .as_ref()
            .unwrap()
            .capabilities()
            .as_ref()
            .unwrap();
        let expected = [Capability::NetBindService]
            .into_iter()
            .collect::<Capabilities>();

        assert_eq!(capabilities.bounding().as_ref(), Some(&expected));
        assert_eq!(capabilities.effective().as_ref(), Some(&expected));
        assert_eq!(capabilities.inheritable().as_ref(), Some(&expected));
        assert_eq!(capabilities.permitted().as_ref(), Some(&expected));
        assert_eq!(capabilities.ambient().as_ref(), Some(&expected));
    }
}
