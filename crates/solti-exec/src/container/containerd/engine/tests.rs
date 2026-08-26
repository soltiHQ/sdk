use containerd_client::tonic::Code;

use super::*;
use crate::container::ContainerErrorClass;

#[test]
fn runtime_status_aggregates_both_local_domains() {
    let healthy = ContainerdWorkerStatus::new(true, true, 1, 4, false);
    let closed = ContainerdWorkerStatus::new(false, true, 0, 4, false);
    let unhealthy = ContainerdWorkerStatus::new(true, false, 2, 4, true);

    let closing = ContainerdRuntimeStatus::new(healthy, closed);
    assert!(!closing.accepting());
    assert!(closing.healthy());
    assert_eq!(closing.cleanup(), healthy);
    assert_eq!(closing.io(), closed);

    let failed = ContainerdRuntimeStatus::new(healthy, unhealthy);
    assert!(failed.accepting());
    assert!(!failed.healthy());
}

#[derive(Default)]
struct RetryCleanupState {
    calls: Vec<tokio::time::Instant>,
    retryable_failures: usize,
}

fn retry_then_succeed(state: &mut RetryCleanupState) -> CleanupOperation<'_> {
    Box::pin(async move {
        state.calls.push(tokio::time::Instant::now());
        if state.retryable_failures == 0 {
            Ok(())
        } else {
            state.retryable_failures -= 1;
            Err(ContainerEngineError::retryable("temporary cleanup failure"))
        }
    })
}

fn fail_permanently(state: &mut RetryCleanupState) -> CleanupOperation<'_> {
    Box::pin(async move {
        state.calls.push(tokio::time::Instant::now());
        Err(ContainerEngineError::permanent("permanent cleanup failure"))
    })
}

#[test]
fn shutdown_combines_both_domain_failures() {
    let error = combine_shutdown_results(
        Err(ContainerEngineError::retryable("cleanup deadline")),
        Err(ContainerEngineError::permanent("I/O ownership lost")),
    )
    .expect_err("both failed domains must fail engine shutdown");

    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd engine shutdown failed");
    assert_eq!(
        error.source().map(ToString::to_string).as_deref(),
        Some("remote cleanup: cleanup deadline; local I/O: I/O ownership lost"),
    );
}

#[derive(Default)]
struct BudgetCleanupState {
    calls: Vec<tokio::time::Instant>,
}

fn slow_then_pending(state: &mut BudgetCleanupState) -> CleanupOperation<'_> {
    Box::pin(async move {
        state.calls.push(tokio::time::Instant::now());
        if state.calls.len() == 1 {
            tokio::time::sleep(Duration::from_secs(20)).await;
            Err(ContainerEngineError::retryable("temporary cleanup failure"))
        } else {
            std::future::pending().await
        }
    })
}

#[tokio::test(start_paused = true)]
async fn retryable_cleanup_uses_bounded_exponential_backoff() {
    let mut state = RetryCleanupState {
        retryable_failures: 2,
        ..Default::default()
    };

    retry_cleanup(&mut state, Duration::from_secs(30), retry_then_succeed)
        .await
        .unwrap();

    assert_eq!(state.calls.len(), 3);
    assert_eq!(state.calls[1] - state.calls[0], CLEANUP_BACKOFF_INITIAL);
    assert_eq!(state.calls[2] - state.calls[1], CLEANUP_BACKOFF_INITIAL * 2);
}

#[tokio::test(start_paused = true)]
async fn permanent_cleanup_failure_is_not_retried() {
    let mut state = RetryCleanupState::default();

    let error = retry_cleanup(&mut state, Duration::from_secs(30), fail_permanently)
        .await
        .unwrap_err();

    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(state.calls.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn cleanup_retries_share_one_total_budget() {
    let mut state = BudgetCleanupState::default();
    let started = tokio::time::Instant::now();

    let error = retry_cleanup(&mut state, Duration::from_secs(30), slow_then_pending)
        .await
        .unwrap_err();

    assert_eq!(error.class(), ContainerErrorClass::Retryable);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(30)
    );
    assert_eq!(state.calls.len(), 2);
}

#[test]
fn only_containerd_major_two_is_accepted() {
    for version in ["2", "2.0.0", "v2.1.4", "  v2.0.0-beta.1  "] {
        assert!(validate_version(version).is_ok(), "{version}");
    }
    for version in ["", "v", "1.7.27", "v3.0.0", "main"] {
        assert!(validate_version(version).is_err(), "{version}");
    }
}

#[test]
fn runtime_info_accepts_containerd_and_canonical_any_type_urls() {
    let runtime = containerd_client::types::RuntimeInfo {
        name: "io.containerd.runc.v2".into(),
        ..Default::default()
    };
    let value = runtime.encode_to_vec();

    for type_url in [
        RUNTIME_INFO_TYPE,
        "/containerd.types.RuntimeInfo",
        "type.googleapis.com/containerd.types.RuntimeInfo",
    ] {
        let decoded = decode_runtime_info(Any {
            type_url: type_url.into(),
            value: value.clone(),
        })
        .unwrap();
        assert_eq!(decoded.name, runtime.name);
    }

    assert!(
        decode_runtime_info(Any {
            type_url: "containerd.types.RuntimeRequest".into(),
            value,
        })
        .is_err()
    );
}

#[test]
fn status_mapping_distinguishes_contract_and_transport_failures() {
    for code in [
        Code::InvalidArgument,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::Unauthenticated,
        Code::FailedPrecondition,
        Code::OutOfRange,
        Code::Unimplemented,
    ] {
        let error = image::rpc_error("operation failed", Status::new(code, "test"));
        assert_eq!(error.class(), ContainerErrorClass::Permanent, "{code:?}");
    }
    for code in [
        Code::Cancelled,
        Code::Unknown,
        Code::DeadlineExceeded,
        Code::ResourceExhausted,
        Code::Aborted,
        Code::Internal,
        Code::Unavailable,
        Code::DataLoss,
    ] {
        let error = image::rpc_error("operation failed", Status::new(code, "test"));
        assert_eq!(error.class(), ContainerErrorClass::Retryable, "{code:?}");
    }
}

#[test]
fn resource_ids_are_attempt_scoped_and_metadata_safe() {
    let ids = ResourceIdGenerator::from_session([0xab; SESSION_BYTES]);

    let first = ids.next().unwrap();
    let second = ids.next().unwrap();

    assert_eq!(
        first,
        "solti-abababababababababababababababab-0000000000000001"
    );
    assert_eq!(
        second,
        "solti-abababababababababababababababab-0000000000000002"
    );
    assert!(
        first
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    );
}

#[test]
fn ownership_labels_identify_only_the_attempt() {
    let request = ContainerRequest {
        attempt_id: "run-secret-a3".to_owned(),
        task_name: solti_model::TaskId::new("task-a").unwrap(),
        generation: 7,
        attempt: 3,
        image: "registry.invalid/image:tag".to_owned(),
        command: Some(vec!["secret-command".to_owned()]),
        args: vec!["secret-argument".to_owned()],
        env: std::collections::BTreeMap::from([("SECRET".to_owned(), "value".to_owned())]),
        process_policy: crate::container::ContainerProcessPolicy::new(),
    };

    let labels = attempt_labels(&request, "resource-1", "session-1");

    assert_eq!(labels.len(), 6);
    assert_eq!(labels[LABEL_MANAGED_BY], MANAGED_BY);
    assert_eq!(labels[LABEL_SESSION], "session-1");
    assert_eq!(labels[LABEL_RESOURCE_ID], "resource-1");
    assert_eq!(labels[LABEL_TASK], "task-a");
    assert_eq!(labels[LABEL_GENERATION], "7");
    assert_eq!(labels[LABEL_ATTEMPT], "3");
    assert!(!labels.values().any(|value| value.contains("secret")));

    let mut changed = labels.clone();
    changed.insert(LABEL_SESSION.to_owned(), "another-session".to_owned());
    assert!(has_ownership_labels(&labels, &labels));
    assert!(!has_ownership_labels(&changed, &labels));
}

#[test]
fn snapshot_identity_requires_our_id_parent_and_labels() {
    let expected_labels = HashMap::from([
        (LABEL_MANAGED_BY.to_owned(), MANAGED_BY.to_owned()),
        (LABEL_SESSION.to_owned(), "session-1".to_owned()),
    ]);
    let mut actual_labels = expected_labels.clone();
    actual_labels.insert("containerd.io/unrelated".to_owned(), "value".to_owned());

    assert!(snapshot_identity_matches(
        "resource-1",
        "parent-1",
        &actual_labels,
        "resource-1",
        "parent-1",
        &expected_labels,
    ));
    assert!(!snapshot_identity_matches(
        "resource-1",
        "another-parent",
        &actual_labels,
        "resource-1",
        "parent-1",
        &expected_labels,
    ));
    assert!(!snapshot_identity_matches(
        "foreign-resource",
        "parent-1",
        &actual_labels,
        "resource-1",
        "parent-1",
        &expected_labels,
    ));
    assert!(!snapshot_identity_matches(
        "resource-1",
        "foreign-parent",
        &actual_labels,
        "resource-1",
        "parent-1",
        &expected_labels,
    ));

    actual_labels.insert(LABEL_SESSION.to_owned(), "foreign-session".to_owned());
    assert!(!snapshot_identity_matches(
        "resource-1",
        "parent-1",
        &actual_labels,
        "resource-1",
        "parent-1",
        &expected_labels,
    ));
}

#[test]
fn container_identity_requires_our_snapshot_binding_and_labels() {
    let expected_labels = HashMap::from([
        (LABEL_MANAGED_BY.to_owned(), MANAGED_BY.to_owned()),
        (LABEL_SESSION.to_owned(), "session-1".to_owned()),
    ]);
    let mut actual_labels = expected_labels.clone();
    actual_labels.insert("containerd.io/unrelated".to_owned(), "value".to_owned());

    assert!(container_identity_matches(
        "resource-1",
        "overlayfs",
        "resource-1",
        &actual_labels,
        "resource-1",
        "overlayfs",
        &expected_labels,
    ));
    for (id, snapshotter, snapshot_key) in [
        ("foreign-resource", "overlayfs", "resource-1"),
        ("resource-1", "foreign-snapshotter", "resource-1"),
        ("resource-1", "overlayfs", "foreign-snapshot"),
    ] {
        assert!(!container_identity_matches(
            id,
            snapshotter,
            snapshot_key,
            &actual_labels,
            "resource-1",
            "overlayfs",
            &expected_labels,
        ));
    }

    actual_labels.insert(LABEL_SESSION.to_owned(), "foreign-session".to_owned());
    assert!(!container_identity_matches(
        "resource-1",
        "overlayfs",
        "resource-1",
        &actual_labels,
        "resource-1",
        "overlayfs",
        &expected_labels,
    ));
}

#[test]
fn task_identity_requires_our_container() {
    let matches = |ownership, container_id, id, pid, stdout, stderr| {
        task_identity_matches(
            ownership,
            TaskIdentity {
                container_id,
                id,
                pid,
                stdout,
                stderr,
            },
            ExpectedTaskIdentity {
                resource_id: "resource-1",
                pid: Some(42),
                stdout: Some("/stdout"),
                stderr: Some("/stderr"),
            },
        )
    };
    for ownership in [
        Ownership::Absent,
        Ownership::Foreign,
        Ownership::CreateUncertain,
        Ownership::DeleteUncertain,
    ] {
        assert!(!matches(
            ownership,
            "",
            "resource-1",
            42,
            "/stdout",
            "/stderr",
        ));
    }
    assert!(matches(
        Ownership::Owned,
        "",
        "resource-1",
        42,
        "/stdout",
        "/stderr",
    ));
    assert!(matches(
        Ownership::Owned,
        "resource-1",
        "resource-1",
        42,
        "/stdout",
        "/stderr",
    ));
    assert!(!matches(
        Ownership::Owned,
        "foreign-resource",
        "resource-1",
        42,
        "/stdout",
        "/stderr",
    ));
    assert!(!matches(
        Ownership::Owned,
        "",
        "foreign-resource",
        42,
        "/stdout",
        "/stderr",
    ));
    assert!(!matches(Ownership::Owned, "", "", 42, "/stdout", "/stderr",));
    assert!(!matches(
        Ownership::Owned,
        "",
        "resource-1",
        99,
        "/stdout",
        "/stderr",
    ));
    assert!(!matches(
        Ownership::Owned,
        "",
        "resource-1",
        42,
        "/foreign/stdout",
        "/stderr",
    ));
    assert!(!matches(
        Ownership::Owned,
        "",
        "resource-1",
        42,
        "/stdout",
        "/foreign/stderr",
    ));
}

#[test]
fn ownership_transitions_preserve_uncertain_failures() {
    assert_eq!(
        ownership_after_read_back(Ownership::CreateUncertain, OwnershipReadBack::Missing),
        Ownership::CreateUncertain,
    );
    assert_eq!(
        ownership_after_read_back(Ownership::DeleteUncertain, OwnershipReadBack::Missing),
        Ownership::Absent,
    );
    assert_eq!(
        ownership_after_read_back(Ownership::CreateUncertain, OwnershipReadBack::Matching),
        Ownership::Owned,
    );
    assert_eq!(
        ownership_after_read_back(Ownership::CreateUncertain, OwnershipReadBack::Mismatched,),
        Ownership::Foreign,
    );
    assert_eq!(
        ownership_after_read_back(Ownership::CreateUncertain, OwnershipReadBack::Unavailable,),
        Ownership::CreateUncertain,
    );
}

#[test]
fn cleanup_eligibility_is_dependency_safe_for_every_ownership_state() {
    let ownerships = [
        Ownership::Absent,
        Ownership::Foreign,
        Ownership::Owned,
        Ownership::CreateUncertain,
        Ownership::DeleteUncertain,
    ];

    for task in ownerships {
        for container in ownerships {
            for snapshot in ownerships {
                let task_absent = task == Ownership::Absent;
                let container_absent = container == Ownership::Absent;
                let expected = CleanupEligibility {
                    confirm_task: matches!(
                        task,
                        Ownership::Owned | Ownership::CreateUncertain | Ownership::DeleteUncertain
                    ),
                    delete_task: task == Ownership::Owned,
                    confirm_container: task_absent
                        && matches!(
                            container,
                            Ownership::Owned
                                | Ownership::CreateUncertain
                                | Ownership::DeleteUncertain
                        ),
                    delete_container: task_absent && container == Ownership::Owned,
                    confirm_snapshot: task_absent
                        && container_absent
                        && matches!(
                            snapshot,
                            Ownership::Owned
                                | Ownership::CreateUncertain
                                | Ownership::DeleteUncertain
                        ),
                    delete_snapshot: task_absent
                        && container_absent
                        && snapshot == Ownership::Owned,
                    cleanup_io: matches!(task, Ownership::Absent | Ownership::Foreign),
                };

                assert_eq!(
                    cleanup_eligibility(task, container, snapshot, true),
                    expected,
                    "task={task:?}, container={container:?}, snapshot={snapshot:?}",
                );
            }
        }
    }
}

#[test]
fn cleanup_dependencies_open_only_after_confirmed_removal() {
    let snapshot = Ownership::Owned;
    let mut task = Ownership::Owned;
    let mut container = Ownership::Owned;

    task = ownership_after_delete_result::<()>(task, &Err(Status::unavailable("test")));
    assert!(!cleanup_eligibility(task, container, snapshot, true).delete_container);
    assert!(!cleanup_eligibility(task, container, snapshot, true).cleanup_io);

    task = ownership_after_delete_result::<()>(task, &Err(Status::not_found("test")));
    assert!(cleanup_eligibility(task, container, snapshot, true).confirm_task);
    task = ownership_after_read_back(task, OwnershipReadBack::Missing);
    assert!(cleanup_eligibility(task, container, snapshot, true).delete_container);

    container = ownership_after_delete_result::<()>(container, &Err(Status::unavailable("test")));
    assert!(!cleanup_eligibility(task, container, snapshot, true).delete_snapshot);

    container = ownership_after_delete_result(container, &Ok(()));
    assert!(cleanup_eligibility(task, container, snapshot, true).confirm_container);
    container = ownership_after_read_back(container, OwnershipReadBack::Missing);
    assert!(cleanup_eligibility(task, container, snapshot, true).delete_snapshot);
    assert!(cleanup_eligibility(task, container, snapshot, true).cleanup_io);
    assert!(!cleanup_eligibility(task, container, snapshot, false).cleanup_io);
}

#[test]
fn only_retryable_statuses_have_ambiguous_create_outcomes() {
    for code in [
        Code::Cancelled,
        Code::Unknown,
        Code::DeadlineExceeded,
        Code::ResourceExhausted,
        Code::Aborted,
        Code::Internal,
        Code::Unavailable,
        Code::DataLoss,
    ] {
        assert!(ambiguous_create_status(&Status::new(code, "test")));
    }
    for code in [
        Code::InvalidArgument,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::Unauthenticated,
        Code::FailedPrecondition,
        Code::OutOfRange,
        Code::Unimplemented,
    ] {
        assert!(!ambiguous_create_status(&Status::new(code, "test")));
    }
}

#[test]
fn plugin_platforms_use_oci_normalization() {
    let amd64_v1 = containerd_client::types::Platform {
        os: "linux".to_owned(),
        architecture: "x86_64".to_owned(),
        variant: "v1".to_owned(),
        os_version: String::new(),
    };
    let arm64 = ContainerPlatform::new("linux", "arm64", "");
    let amd64 = ContainerPlatform::new("linux", "amd64", "");

    assert!(platform_matches(&amd64_v1, &amd64));
    assert!(!platform_matches(&amd64_v1, &arm64));
}
