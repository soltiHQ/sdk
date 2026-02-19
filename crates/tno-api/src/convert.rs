use tracing::warn;

use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, Flag, JitterStrategy, RestartStrategy,
    RunnerLabels, TaskEnv, TaskInfo, TaskKind, TaskStatus,
};

use crate::error::ApiError;
use crate::proto_api;

impl From<TaskStatus> for proto_api::TaskStatus {
    fn from(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Pending => proto_api::TaskStatus::Pending,
            TaskStatus::Running => proto_api::TaskStatus::Running,
            TaskStatus::Succeeded => proto_api::TaskStatus::Succeeded,
            TaskStatus::Failed => proto_api::TaskStatus::Failed,
            TaskStatus::Timeout => proto_api::TaskStatus::Timeout,
            TaskStatus::Canceled => proto_api::TaskStatus::Canceled,
            TaskStatus::Exhausted => proto_api::TaskStatus::Exhausted,
        }
    }
}

impl From<TaskInfo> for proto_api::TaskInfo {
    fn from(info: TaskInfo) -> Self {
        use std::time::UNIX_EPOCH;

        let created_at = info
            .created_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|e| {
                warn!(task_id = %info.id, error = %e, "created_at is before unix epoch, defaulting to 0");
                std::time::Duration::ZERO
            })
            .as_secs() as i64;

        let updated_at = info
            .updated_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|e| {
                warn!(task_id = %info.id, error = %e, "updated_at is before unix epoch, defaulting to 0");
                std::time::Duration::ZERO
            })
            .as_secs() as i64;

        proto_api::TaskInfo {
            id: info.id.to_string(),
            slot: info.slot,
            status: proto_api::TaskStatus::from(info.status) as i32,
            attempt: info.attempt,
            created_at,
            updated_at,
            error: info.error,
        }
    }
}

impl TryFrom<proto_api::CreateSpec> for CreateSpec {
    type Error = ApiError;

    fn try_from(spec: proto_api::CreateSpec) -> Result<Self, Self::Error> {
        let kind = spec
            .kind
            .ok_or_else(|| ApiError::InvalidRequest("missing task kind".into()))?
            .kind // добавить .kind для unwrap oneof
            .ok_or_else(|| ApiError::InvalidRequest("missing task kind variant".into()))?;

        let task_kind = convert_task_kind(kind)?;

        let restart = convert_restart_strategy(
            proto_api::RestartStrategy::try_from(spec.restart)
                .map_err(|_| ApiError::InvalidRequest("invalid restart strategy".into()))?,
            spec.restart_interval_ms,
        )?;

        let backoff = spec
            .backoff
            .ok_or_else(|| ApiError::InvalidRequest("missing backoff strategy".into()))?;

        Ok(CreateSpec {
            slot: validate_slot(spec.slot)?,
            kind: task_kind,
            timeout_ms: validate_timeout(spec.timeout_ms)?,
            restart,
            backoff: convert_backoff_strategy(backoff)?,
            admission: convert_admission_strategy(
                proto_api::AdmissionStrategy::try_from(spec.admission)
                    .map_err(|_| ApiError::InvalidRequest("invalid admission strategy".into()))?,
            )?,
            labels: convert_labels(spec.labels),
        })
    }
}

fn convert_task_kind(kind: proto_api::task_kind::Kind) -> Result<TaskKind, ApiError> {
    match kind {
        proto_api::task_kind::Kind::Subprocess(sub) => {
            if sub.command.trim().is_empty() {
                return Err(ApiError::InvalidRequest(
                    "subprocess command is empty".into(),
                ));
            }

            Ok(TaskKind::Subprocess {
                command: sub.command,
                args: sub.args,
                env: convert_env(sub.env),
                cwd: sub.cwd.map(std::path::PathBuf::from),
                fail_on_non_zero: Flag::from(sub.fail_on_non_zero),
            })
        }
        proto_api::task_kind::Kind::Wasm(wasm) => {
            if wasm.module.trim().is_empty() {
                return Err(ApiError::InvalidRequest("wasm module path is empty".into()));
            }

            Ok(TaskKind::Wasm {
                module: std::path::PathBuf::from(wasm.module),
                args: wasm.args,
                env: convert_env(wasm.env),
            })
        }
        proto_api::task_kind::Kind::Container(cont) => {
            if cont.image.trim().is_empty() {
                return Err(ApiError::InvalidRequest("container image is empty".into()));
            }

            Ok(TaskKind::Container {
                image: cont.image,
                command: if cont.command.is_empty() {
                    None
                } else {
                    Some(cont.command)
                },
                args: cont.args,
                env: convert_env(cont.env),
            })
        }
    }
}

fn convert_env(kvs: Vec<proto_api::KeyValue>) -> TaskEnv {
    let mut env = TaskEnv::new();
    for kv in kvs {
        env.push(kv.key, kv.value);
    }
    env
}

fn convert_restart_strategy(
    strategy: proto_api::RestartStrategy,
    interval_ms: Option<u64>,
) -> Result<RestartStrategy, ApiError> {
    match strategy {
        proto_api::RestartStrategy::Never => Ok(RestartStrategy::Never),
        proto_api::RestartStrategy::OnFailure => Ok(RestartStrategy::OnFailure),
        proto_api::RestartStrategy::Always => Ok(RestartStrategy::Always { interval_ms }),
        proto_api::RestartStrategy::Unspecified => Err(ApiError::InvalidRequest(
            "restart strategy not specified".into(),
        )),
    }
}

fn convert_backoff_strategy(
    backoff: proto_api::BackoffStrategy,
) -> Result<BackoffStrategy, ApiError> {
    let jitter = proto_api::JitterStrategy::try_from(backoff.jitter)
        .map_err(|_| ApiError::InvalidRequest("invalid jitter strategy".into()))?;

    let jitter = match jitter {
        proto_api::JitterStrategy::None => JitterStrategy::None,
        proto_api::JitterStrategy::Full => JitterStrategy::Full,
        proto_api::JitterStrategy::Equal => JitterStrategy::Equal,
        proto_api::JitterStrategy::Decorrelated => JitterStrategy::Decorrelated,
        proto_api::JitterStrategy::Unspecified => {
            return Err(ApiError::InvalidRequest(
                "jitter strategy not specified".into(),
            ));
        }
    };

    if backoff.first_ms == 0 {
        return Err(ApiError::InvalidRequest(
            "backoff first_ms cannot be zero".into(),
        ));
    }
    if backoff.max_ms == 0 {
        return Err(ApiError::InvalidRequest(
            "backoff max_ms cannot be zero".into(),
        ));
    }
    if backoff.factor <= 0.0 {
        return Err(ApiError::InvalidRequest(
            "backoff factor must be positive".into(),
        ));
    }

    Ok(BackoffStrategy {
        jitter,
        first_ms: backoff.first_ms,
        max_ms: backoff.max_ms,
        factor: backoff.factor,
    })
}

fn convert_admission_strategy(
    strategy: proto_api::AdmissionStrategy,
) -> Result<AdmissionStrategy, ApiError> {
    match strategy {
        proto_api::AdmissionStrategy::DropIfRunning => Ok(AdmissionStrategy::DropIfRunning),
        proto_api::AdmissionStrategy::Replace => Ok(AdmissionStrategy::Replace),
        proto_api::AdmissionStrategy::Queue => Ok(AdmissionStrategy::Queue),
        proto_api::AdmissionStrategy::Unspecified => Err(ApiError::InvalidRequest(
            "admission strategy not specified".into(),
        )),
    }
}

fn convert_labels(map: std::collections::HashMap<String, String>) -> RunnerLabels {
    let mut labels = RunnerLabels::new();
    for (k, v) in map {
        labels.insert(k, v);
    }
    labels
}

fn validate_slot(slot: String) -> Result<String, ApiError> {
    if slot.trim().is_empty() {
        return Err(ApiError::InvalidRequest("slot cannot be empty".into()));
    }
    Ok(slot)
}

fn validate_timeout(timeout_ms: u64) -> Result<u64, ApiError> {
    if timeout_ms == 0 {
        return Err(ApiError::InvalidRequest("timeout_ms cannot be zero".into()));
    }
    Ok(timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_subprocess_kind(command: &str) -> proto_api::TaskKind {
        proto_api::TaskKind {
            kind: Some(proto_api::task_kind::Kind::Subprocess(
                proto_api::SubprocessTask {
                    command: command.to_string(),
                    args: vec!["-l".to_string()],
                    env: vec![proto_api::KeyValue {
                        key: "PATH".to_string(),
                        value: "/usr/bin".to_string(),
                    }],
                    cwd: Some("/tmp".to_string()),
                    fail_on_non_zero: true,
                },
            )),
        }
    }

    fn make_backoff() -> proto_api::BackoffStrategy {
        proto_api::BackoffStrategy {
            jitter: proto_api::JitterStrategy::Full as i32,
            first_ms: 100,
            max_ms: 10_000,
            factor: 2.0,
        }
    }

    fn make_valid_create_spec() -> proto_api::CreateSpec {
        proto_api::CreateSpec {
            slot: "test-slot".to_string(),
            kind: Some(make_subprocess_kind("ls")),
            timeout_ms: 5_000,
            restart: proto_api::RestartStrategy::OnFailure as i32,
            restart_interval_ms: None,
            backoff: Some(make_backoff()),
            admission: proto_api::AdmissionStrategy::DropIfRunning as i32,
            labels: HashMap::new(),
        }
    }

    #[test]
    fn task_status_all_variants() {
        let cases = [
            (TaskStatus::Pending, proto_api::TaskStatus::Pending),
            (TaskStatus::Running, proto_api::TaskStatus::Running),
            (TaskStatus::Succeeded, proto_api::TaskStatus::Succeeded),
            (TaskStatus::Failed, proto_api::TaskStatus::Failed),
            (TaskStatus::Timeout, proto_api::TaskStatus::Timeout),
            (TaskStatus::Canceled, proto_api::TaskStatus::Canceled),
            (TaskStatus::Exhausted, proto_api::TaskStatus::Exhausted),
        ];

        for (domain, expected_proto) in cases {
            let proto = proto_api::TaskStatus::from(domain);
            assert_eq!(proto, expected_proto, "mismatch for {:?}", domain);
        }
    }

    #[test]
    fn task_info_converts_correctly() {
        let now = SystemTime::now();
        let now_secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;

        let info = TaskInfo {
            id: tno_model::TaskId::from("task-42"),
            slot: "my-slot".to_string(),
            status: TaskStatus::Running,
            attempt: 3,
            created_at: now,
            updated_at: now,
            error: Some("boom".to_string()),
        };

        let proto: proto_api::TaskInfo = info.into();

        assert_eq!(proto.id, "task-42");
        assert_eq!(proto.slot, "my-slot");
        assert_eq!(proto.status, proto_api::TaskStatus::Running as i32);
        assert_eq!(proto.attempt, 3);
        assert_eq!(proto.created_at, now_secs);
        assert_eq!(proto.updated_at, now_secs);
        assert_eq!(proto.error, Some("boom".to_string()));
    }

    #[test]
    fn task_info_no_error() {
        let info = TaskInfo {
            id: tno_model::TaskId::from("task-1"),
            slot: "slot".to_string(),
            status: TaskStatus::Succeeded,
            attempt: 1,
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            error: None,
        };

        let proto: proto_api::TaskInfo = info.into();
        assert_eq!(proto.error, None);
    }

    #[test]
    fn create_spec_subprocess_valid() {
        let spec = make_valid_create_spec();
        let result = CreateSpec::try_from(spec);
        assert!(result.is_ok());

        let cs = result.unwrap();
        assert_eq!(cs.slot, "test-slot");
        assert_eq!(cs.timeout_ms, 5_000);
        assert!(matches!(cs.kind, TaskKind::Subprocess { ref command, .. } if command == "ls"));
        assert!(matches!(cs.restart, RestartStrategy::OnFailure));
        assert!(matches!(cs.admission, AdmissionStrategy::DropIfRunning));
        assert_eq!(cs.backoff.first_ms, 100);
        assert_eq!(cs.backoff.max_ms, 10_000);
    }

    #[test]
    fn create_spec_wasm_valid() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Wasm(proto_api::WasmTask {
                    module: "/app/module.wasm".to_string(),
                    args: vec!["--verbose".to_string()],
                    env: vec![],
                })),
            }),
            ..make_valid_create_spec()
        };

        let cs = CreateSpec::try_from(spec).unwrap();
        assert!(
            matches!(cs.kind, TaskKind::Wasm { ref module, .. } if module.to_str() == Some("/app/module.wasm"))
        );
    }

    #[test]
    fn create_spec_container_valid() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Container(
                    proto_api::ContainerTask {
                        image: "alpine:latest".to_string(),
                        command: vec!["sh".to_string(), "-c".to_string()],
                        args: vec!["echo hello".to_string()],
                        env: vec![],
                    },
                )),
            }),
            ..make_valid_create_spec()
        };

        let cs = CreateSpec::try_from(spec).unwrap();
        assert!(
            matches!(cs.kind, TaskKind::Container { ref image, .. } if image == "alpine:latest")
        );
    }

    #[test]
    fn create_spec_container_empty_command_becomes_none() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Container(
                    proto_api::ContainerTask {
                        image: "nginx".to_string(),
                        command: vec![],
                        args: vec![],
                        env: vec![],
                    },
                )),
            }),
            ..make_valid_create_spec()
        };

        let cs = CreateSpec::try_from(spec).unwrap();
        assert!(matches!(cs.kind, TaskKind::Container { command: None, .. }));
    }

    #[test]
    fn create_spec_always_with_interval() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Always as i32,
            restart_interval_ms: Some(5_000),
            ..make_valid_create_spec()
        };

        let cs = CreateSpec::try_from(spec).unwrap();
        assert!(matches!(
            cs.restart,
            RestartStrategy::Always {
                interval_ms: Some(5_000)
            }
        ));
    }

    #[test]
    fn create_spec_always_without_interval() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Always as i32,
            restart_interval_ms: None,
            ..make_valid_create_spec()
        };

        let cs = CreateSpec::try_from(spec).unwrap();
        assert!(matches!(
            cs.restart,
            RestartStrategy::Always { interval_ms: None }
        ));
    }

    #[test]
    fn create_spec_with_labels() {
        let mut labels = HashMap::new();
        labels.insert("runner-tag".to_string(), "gpu".to_string());
        labels.insert("env".to_string(), "prod".to_string());

        let spec = proto_api::CreateSpec {
            labels,
            ..make_valid_create_spec()
        };

        let cs = CreateSpec::try_from(spec).unwrap();
        assert_eq!(cs.labels.get("runner-tag"), Some("gpu"));
        assert_eq!(cs.labels.get("env"), Some("prod"));
    }

    #[test]
    fn create_spec_env_conversion() {
        let spec = make_valid_create_spec();
        let cs = CreateSpec::try_from(spec).unwrap();

        if let TaskKind::Subprocess { ref env, .. } = cs.kind {
            assert_eq!(env.get("PATH"), Some("/usr/bin"));
        } else {
            panic!("expected subprocess kind");
        }
    }

    #[test]
    fn reject_missing_kind() {
        let spec = proto_api::CreateSpec {
            kind: None,
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing task kind")));
    }

    #[test]
    fn reject_missing_kind_variant() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind { kind: None }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing task kind variant"))
        );
    }

    #[test]
    fn reject_empty_subprocess_command() {
        let spec = proto_api::CreateSpec {
            kind: Some(make_subprocess_kind("")),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("subprocess command is empty"))
        );
    }

    #[test]
    fn reject_whitespace_subprocess_command() {
        let spec = proto_api::CreateSpec {
            kind: Some(make_subprocess_kind("   ")),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("subprocess command is empty"))
        );
    }

    #[test]
    fn reject_empty_wasm_module() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Wasm(proto_api::WasmTask {
                    module: "".to_string(),
                    args: vec![],
                    env: vec![],
                })),
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("wasm module path is empty"))
        );
    }

    #[test]
    fn reject_empty_container_image() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Container(
                    proto_api::ContainerTask {
                        image: "".to_string(),
                        command: vec![],
                        args: vec![],
                        env: vec![],
                    },
                )),
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("container image is empty"))
        );
    }

    #[test]
    fn reject_empty_slot() {
        let spec = proto_api::CreateSpec {
            slot: "".to_string(),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("slot cannot be empty"))
        );
    }

    #[test]
    fn reject_whitespace_slot() {
        let spec = proto_api::CreateSpec {
            slot: "   ".to_string(),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("slot cannot be empty"))
        );
    }

    #[test]
    fn reject_zero_timeout() {
        let spec = proto_api::CreateSpec {
            timeout_ms: 0,
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("timeout_ms cannot be zero"))
        );
    }

    #[test]
    fn reject_missing_backoff() {
        let spec = proto_api::CreateSpec {
            backoff: None,
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing backoff")));
    }

    #[test]
    fn reject_zero_backoff_first_ms() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffStrategy {
                first_ms: 0,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("first_ms cannot be zero"))
        );
    }

    #[test]
    fn reject_zero_backoff_max_ms() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffStrategy {
                max_ms: 0,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("max_ms cannot be zero"))
        );
    }

    #[test]
    fn reject_negative_backoff_factor() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffStrategy {
                factor: -1.0,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("factor must be positive"))
        );
    }

    #[test]
    fn reject_zero_backoff_factor() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffStrategy {
                factor: 0.0,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("factor must be positive"))
        );
    }

    #[test]
    fn reject_unspecified_jitter() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffStrategy {
                jitter: proto_api::JitterStrategy::Unspecified as i32,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("jitter")));
    }

    #[test]
    fn all_jitter_strategies_convert() {
        let cases = [
            (proto_api::JitterStrategy::None, JitterStrategy::None),
            (proto_api::JitterStrategy::Full, JitterStrategy::Full),
            (proto_api::JitterStrategy::Equal, JitterStrategy::Equal),
            (
                proto_api::JitterStrategy::Decorrelated,
                JitterStrategy::Decorrelated,
            ),
        ];

        for (proto_jitter, expected) in cases {
            let spec = proto_api::CreateSpec {
                backoff: Some(proto_api::BackoffStrategy {
                    jitter: proto_jitter as i32,
                    ..make_backoff()
                }),
                ..make_valid_create_spec()
            };
            let cs = CreateSpec::try_from(spec).unwrap();
            assert_eq!(cs.backoff.jitter, expected);
        }
    }

    #[test]
    fn reject_unspecified_restart() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Unspecified as i32,
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("restart")));
    }

    #[test]
    fn restart_never() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Never as i32,
            ..make_valid_create_spec()
        };
        let cs = CreateSpec::try_from(spec).unwrap();
        assert!(matches!(cs.restart, RestartStrategy::Never));
    }

    #[test]
    fn reject_unspecified_admission() {
        let spec = proto_api::CreateSpec {
            admission: proto_api::AdmissionStrategy::Unspecified as i32,
            ..make_valid_create_spec()
        };
        let err = CreateSpec::try_from(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("admission")));
    }

    #[test]
    fn all_admission_strategies_convert() {
        let cases = [
            (
                proto_api::AdmissionStrategy::DropIfRunning,
                AdmissionStrategy::DropIfRunning,
            ),
            (
                proto_api::AdmissionStrategy::Replace,
                AdmissionStrategy::Replace,
            ),
            (
                proto_api::AdmissionStrategy::Queue,
                AdmissionStrategy::Queue,
            ),
        ];

        for (proto_adm, expected) in cases {
            let spec = proto_api::CreateSpec {
                admission: proto_adm as i32,
                ..make_valid_create_spec()
            };
            let cs = CreateSpec::try_from(spec).unwrap();
            assert_eq!(cs.admission, expected);
        }
    }
}
