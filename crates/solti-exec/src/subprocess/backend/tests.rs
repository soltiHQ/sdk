use super::*;

#[cfg(target_os = "linux")]
#[test]
fn credential_change_requires_process_group_termination_authority() {
    assert!(!credential_termination_authority_required(
        None, 1000, 2000, false
    ));
    assert!(!credential_termination_authority_required(
        Some(1000),
        1000,
        2000,
        false
    ));
    assert!(!credential_termination_authority_required(
        Some(2000),
        1000,
        2000,
        false
    ));
    assert!(credential_termination_authority_required(
        Some(3000),
        1000,
        2000,
        false
    ));
    assert!(credential_termination_authority_required(
        None, 1000, 2000, true
    ));
    assert!(credential_termination_authority_required(
        Some(1000),
        1000,
        2000,
        true
    ));
}

#[test]
fn invalid_log_limits_are_rejected() {
    let cases = [
        (
            LogConfig {
                max_line_length: 0,
                ..LogConfig::default()
            },
            "max_line_length",
        ),
        (
            LogConfig {
                max_line_bytes: 0,
                ..LogConfig::default()
            },
            "max_line_bytes",
        ),
    ];

    for (logger, expected) in cases {
        let error = SubprocessBackendConfig::new()
            .with_logger(logger)
            .prepare()
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "got {error:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn host_process_policy_is_applied_to_spawned_command() {
    use crate::host::{HostProcessPolicy, RlimitConfig};

    let requested = crate::host::reduced_nofile_limit_for_test();
    let config = SubprocessBackendConfig::new()
        .with_host_process_policy(HostProcessPolicy::new().with_rlimits(RlimitConfig {
            max_open_files: Some(requested),
            ..Default::default()
        }))
        .prepare()
        .unwrap();
    let attempt = config.prepare_host_process_attempt(None).unwrap();
    let mut command = Command::new("sh");
    command.arg("-c").arg("ulimit -n");
    let _guard = config.apply_to_command(&mut command, attempt);
    let output = command.output().await.unwrap();

    assert!(output.status.success());
    let actual = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    assert_eq!(actual, requested);
}

#[test]
fn max_script_body_bytes_defaults_to_model_const_and_is_configurable() {
    use solti_model::MAX_SCRIPT_BODY_BYTES;

    let default_cfg = SubprocessBackendConfig::new().prepare().unwrap();
    assert_eq!(default_cfg.max_script_body_bytes(), MAX_SCRIPT_BODY_BYTES);

    let custom = SubprocessBackendConfig::new()
        .with_max_script_body_bytes(4096)
        .prepare()
        .unwrap();
    assert_eq!(custom.max_script_body_bytes(), 4096);
}

#[test]
fn cleanup_capacity_defaults_and_validation_are_explicit() {
    let default_cfg = SubprocessBackendConfig::new();
    assert_eq!(
        default_cfg.cleanup_capacity(),
        DEFAULT_SUBPROCESS_CLEANUP_CAPACITY
    );
    assert_eq!(
        default_cfg.prepare().unwrap().prepared_cleanup_capacity(),
        DEFAULT_SUBPROCESS_CLEANUP_CAPACITY
    );

    let configured = SubprocessBackendConfig::new().with_cleanup_capacity(7);
    assert_eq!(configured.cleanup_capacity(), 7);
    assert_eq!(configured.prepare().unwrap().prepared_cleanup_capacity(), 7);

    assert!(
        SubprocessBackendConfig::new()
            .with_cleanup_capacity(0)
            .prepare()
            .is_err()
    );
    if let Ok(unsupported) = usize::try_from(u64::from(u32::MAX) + 1) {
        assert!(
            SubprocessBackendConfig::new()
                .with_cleanup_capacity(unsupported)
                .prepare()
                .is_err()
        );
    }
}

#[test]
fn invalid_script_body_limits_are_rejected() {
    for max in [0, MAX_SCRIPT_BODY_BYTES + 1] {
        let error = SubprocessBackendConfig::new()
            .with_max_script_body_bytes(max)
            .prepare()
            .unwrap_err()
            .to_string();
        assert!(error.contains("1..="), "limit {max}: got {error:?}");
    }
}

#[test]
fn env_policy_defaults_to_clear() {
    let cfg = SubprocessBackendConfig::new().prepare().unwrap();
    assert!(matches!(cfg.env_policy(), EnvPolicy::Clear));
}

#[test]
fn invalid_allowlist_key_is_rejected() {
    let config = SubprocessBackendConfig::new()
        .with_env_policy(EnvPolicy::Allowlist(vec!["BAD=KEY".into()]));
    let error = config.prepare().unwrap_err().to_string();
    assert!(error.contains("environment variable name"), "got: {error}");
}

#[test]
fn cwd_unrestricted_allows_inherited_or_existing_directory() {
    let cfg = SubprocessBackendConfig::new().prepare().unwrap();
    assert!(cfg.pin_cwd(None).unwrap().is_none());

    let cwd = tempfile::TempDir::new().unwrap();
    assert!(cfg.pin_cwd(Some(cwd.path())).unwrap().is_some());
}

#[test]
fn cwd_unrestricted_rejects_nonexistent_directory() {
    let cfg = SubprocessBackendConfig::new().prepare().unwrap();
    let error = cfg
        .pin_cwd(Some(Path::new("/nonexistent/solti-cwd")))
        .unwrap_err();
    assert!(error.contains("cannot be resolved"), "got: {error}");
}

#[test]
fn cwd_roots_allows_paths_inside() {
    let root = tempfile::TempDir::new().unwrap();
    let sub = root.path().join("work");
    std::fs::create_dir(&sub).unwrap();

    let cfg = SubprocessBackendConfig::new()
        .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
        .prepare()
        .unwrap();

    assert!(cfg.pin_cwd(Some(&sub)).unwrap().is_some());
    assert!(cfg.pin_cwd(Some(root.path())).unwrap().is_some());
}

#[test]
fn cwd_roots_requires_explicit_cwd() {
    let root = tempfile::TempDir::new().unwrap();
    let cfg = SubprocessBackendConfig::new()
        .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
        .prepare()
        .unwrap();

    let err = cfg.pin_cwd(None).unwrap_err().to_string();
    assert!(err.contains("cwd is required"), "got: {err}");
}

#[test]
fn cwd_roots_rejects_paths_outside() {
    let root = tempfile::TempDir::new().unwrap();
    let other = tempfile::TempDir::new().unwrap();

    let cfg = SubprocessBackendConfig::new()
        .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
        .prepare()
        .unwrap();

    let err = cfg.pin_cwd(Some(other.path())).unwrap_err().to_string();
    assert!(err.contains("outside the allowed roots"), "got: {err}");
}

#[test]
fn cwd_roots_rejects_nonexistent() {
    let root = tempfile::TempDir::new().unwrap();
    let cfg = SubprocessBackendConfig::new()
        .with_cwd_policy(CwdPolicy::Roots(vec![root.path().to_path_buf()]))
        .prepare()
        .unwrap();

    let missing = root.path().join("does-not-exist");
    let err = cfg.pin_cwd(Some(&missing)).unwrap_err().to_string();
    assert!(err.contains("cannot be resolved"), "got: {err}");
}

#[test]
fn cwd_roots_rejects_traversal_escape() {
    // A cwd built to look like it is under the root but that resolves out of
    // it via `..` must be rejected: canonicalize collapses the traversal.
    let base = tempfile::TempDir::new().unwrap();
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();

    let cfg = SubprocessBackendConfig::new()
        .with_cwd_policy(CwdPolicy::Roots(vec![root.clone()]))
        .prepare()
        .unwrap();

    let escape = root.join("..").join("outside");
    let err = cfg.pin_cwd(Some(&escape)).unwrap_err().to_string();
    assert!(err.contains("outside the allowed roots"), "got: {err}");
}

#[test]
fn cwd_root_must_exist() {
    let config = SubprocessBackendConfig::new()
        .with_cwd_policy(CwdPolicy::Roots(vec![PathBuf::from("/missing/solti-root")]));
    let error = config.prepare().unwrap_err().to_string();
    assert!(error.contains("cannot be resolved"), "got: {error}");
}

#[cfg(unix)]
#[test]
fn passed_fd_is_owned_by_prepared_backend() {
    use std::os::fd::AsRawFd as _;

    let file = tempfile::tempfile().unwrap();
    let expected = file.as_raw_fd();
    let cfg = SubprocessBackendConfig::new()
        .with_passed_fd(file.into())
        .prepare()
        .unwrap();

    assert_eq!(cfg.passed_fds(), vec![expected]);
}
