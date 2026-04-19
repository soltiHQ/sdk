//! # `TaskSpec` ↔ `CreateSpec` conversion.
//!
//! This module covers both directions:
//!
//! | Direction     | Entry point              | Shape                                      |
//! |---------------|--------------------------|--------------------------------------------|
//! | Domain → wire | [`spec_to_proto`]        | used by `task.rs` when building `TaskData` |
//! | Wire → domain | [`convert_create_spec`]  | gRPC/HTTP submit path                      |

use solti_model::{
    AdmissionPolicy, BackoffPolicy, ContainerSpec, Flag, JitterPolicy, Labels, RestartPolicy,
    Runtime, Slot, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec, WasmSpec,
};

use crate::error::ApiError;
use crate::proto_api;
use crate::validate::{validate_slot, validate_timeout};

/// Build a proto [`proto_api::CreateSpec`] from a domain [`TaskSpec`].
pub(super) fn spec_to_proto(spec: &TaskSpec) -> Result<proto_api::CreateSpec, ApiError> {
    let (restart, restart_interval_ms) = restart_to_proto(spec.restart());
    Ok(proto_api::CreateSpec {
        admission: admission_to_proto(spec.admission()) as i32,
        backoff: Some(backoff_to_proto(spec.backoff())),
        kind: Some(kind_to_proto(spec.kind())?),
        timeout_ms: spec.timeout().as_millis(),
        slot: spec.slot().to_string(),
        restart: restart as i32,
        restart_interval_ms,
        labels: spec
            .labels()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    })
}

fn kind_to_proto(kind: &TaskKind) -> Result<proto_api::TaskKind, ApiError> {
    let inner = match kind {
        TaskKind::Subprocess(sub) => {
            let mode = match &sub.mode {
                SubprocessMode::Command { command, args } => {
                    proto_api::subprocess_task::Mode::Command(proto_api::CommandMode {
                        command: command.clone(),
                        args: args.clone(),
                    })
                }
                SubprocessMode::Script {
                    runtime,
                    body,
                    args,
                } => {
                    let runtime_proto = match runtime {
                        Runtime::Bash => proto_api::script_mode::Runtime::WellKnown(
                            proto_api::ScriptRuntime::Bash as i32,
                        ),
                        Runtime::Python => proto_api::script_mode::Runtime::WellKnown(
                            proto_api::ScriptRuntime::Python as i32,
                        ),
                        Runtime::Node => proto_api::script_mode::Runtime::WellKnown(
                            proto_api::ScriptRuntime::Node as i32,
                        ),
                        Runtime::Custom { command, flag } => {
                            proto_api::script_mode::Runtime::Custom(proto_api::CustomRuntime {
                                command: command.clone(),
                                flag: flag.clone(),
                            })
                        }
                    };
                    proto_api::subprocess_task::Mode::Script(proto_api::ScriptMode {
                        runtime: Some(runtime_proto),
                        body: body.clone(),
                        args: args.clone(),
                    })
                }
            };
            proto_api::task_kind::Kind::Subprocess(proto_api::SubprocessTask {
                mode: Some(mode),
                env: env_to_proto(&sub.env),
                cwd: sub.cwd.as_ref().map(|p| p.to_string_lossy().to_string()),
                fail_on_non_zero: sub.fail_on_non_zero.into(),
            })
        }
        TaskKind::Wasm(w) => proto_api::task_kind::Kind::Wasm(proto_api::WasmTask {
            module: w.module.to_string_lossy().to_string(),
            env: env_to_proto(&w.env),
            args: w.args.clone(),
        }),
        TaskKind::Container(c) => proto_api::task_kind::Kind::Container(proto_api::ContainerTask {
            command: c.command.clone().unwrap_or_default(),
            env: env_to_proto(&c.env),
            image: c.image.clone(),
            args: c.args.clone(),
        }),
        TaskKind::Embedded => {
            return Err(ApiError::InvalidRequest(
                "embedded tasks have no wire representation and cannot cross the API boundary"
                    .into(),
            ));
        }
        other => {
            return Err(ApiError::Internal(format!(
                "unsupported task kind variant: {:?}",
                other
            )));
        }
    };
    Ok(proto_api::TaskKind { kind: Some(inner) })
}

fn env_to_proto(env: &TaskEnv) -> Vec<proto_api::KeyValue> {
    env.iter()
        .map(|kv| proto_api::KeyValue {
            key: kv.key().to_string(),
            value: kv.value().to_string(),
        })
        .collect()
}

fn restart_to_proto(policy: RestartPolicy) -> (proto_api::RestartStrategy, Option<u64>) {
    match policy {
        RestartPolicy::Never => (proto_api::RestartStrategy::Never, None),
        RestartPolicy::OnFailure => (proto_api::RestartStrategy::OnFailure, None),
        RestartPolicy::Always { interval_ms } => (proto_api::RestartStrategy::Always, interval_ms),
        _ => (proto_api::RestartStrategy::Unspecified, None),
    }
}

fn backoff_to_proto(b: &BackoffPolicy) -> proto_api::BackoffStrategy {
    let jitter = match b.jitter {
        JitterPolicy::None => proto_api::JitterStrategy::None,
        JitterPolicy::Full => proto_api::JitterStrategy::Full,
        JitterPolicy::Equal => proto_api::JitterStrategy::Equal,
        JitterPolicy::Decorrelated => proto_api::JitterStrategy::Decorrelated,
        _ => proto_api::JitterStrategy::Unspecified,
    };
    proto_api::BackoffStrategy {
        jitter: jitter as i32,
        first_ms: b.first_ms,
        max_ms: b.max_ms,
        factor: b.factor,
    }
}

fn admission_to_proto(policy: AdmissionPolicy) -> proto_api::AdmissionStrategy {
    match policy {
        AdmissionPolicy::DropIfRunning => proto_api::AdmissionStrategy::DropIfRunning,
        AdmissionPolicy::Replace => proto_api::AdmissionStrategy::Replace,
        AdmissionPolicy::Queue => proto_api::AdmissionStrategy::Queue,
        _ => proto_api::AdmissionStrategy::Unspecified,
    }
}

/// Convert a proto [`proto_api::CreateSpec`] into a domain [`TaskSpec`].
pub fn convert_create_spec(spec: proto_api::CreateSpec) -> Result<TaskSpec, ApiError> {
    let slot: Slot = validate_slot(spec.slot)?.into();

    let kind = spec
        .kind
        .ok_or_else(|| ApiError::InvalidRequest("missing task kind".into()))?
        .kind
        .ok_or_else(|| ApiError::InvalidRequest("missing task kind variant".into()))?;

    let task_kind = convert_task_kind(kind)?;

    let restart = convert_restart_policy(
        proto_api::RestartStrategy::try_from(spec.restart)
            .map_err(|_| ApiError::InvalidRequest("invalid restart strategy".into()))?,
        spec.restart_interval_ms,
    )?;

    let backoff = spec
        .backoff
        .ok_or_else(|| ApiError::InvalidRequest("missing backoff strategy".into()))?;

    let task_spec = TaskSpec::builder(slot, task_kind, validate_timeout(spec.timeout_ms)?)
        .restart(restart)
        .backoff(convert_backoff_policy(backoff)?)
        .admission(convert_admission_policy(
            proto_api::AdmissionStrategy::try_from(spec.admission)
                .map_err(|_| ApiError::InvalidRequest("invalid admission strategy".into()))?,
        )?)
        .labels(convert_labels(spec.labels))
        .build()
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    Ok(task_spec)
}

fn convert_task_kind(kind: proto_api::task_kind::Kind) -> Result<TaskKind, ApiError> {
    match kind {
        proto_api::task_kind::Kind::Subprocess(sub) => {
            let mode = sub
                .mode
                .ok_or_else(|| ApiError::InvalidRequest("missing subprocess mode".into()))?;

            let subprocess_mode = match mode {
                proto_api::subprocess_task::Mode::Command(cmd) => SubprocessMode::Command {
                    command: cmd.command,
                    args: cmd.args,
                },
                proto_api::subprocess_task::Mode::Script(script) => {
                    let runtime = script
                        .runtime
                        .ok_or_else(|| ApiError::InvalidRequest("missing script runtime".into()))?;

                    let runtime = match runtime {
                        proto_api::script_mode::Runtime::WellKnown(val) => {
                            match proto_api::ScriptRuntime::try_from(val) {
                                Ok(proto_api::ScriptRuntime::Bash) => Runtime::Bash,
                                Ok(proto_api::ScriptRuntime::Python) => Runtime::Python,
                                Ok(proto_api::ScriptRuntime::Node) => Runtime::Node,
                                _ => {
                                    return Err(ApiError::InvalidRequest(
                                        "unknown or unspecified script runtime".into(),
                                    ));
                                }
                            }
                        }
                        proto_api::script_mode::Runtime::Custom(c) => Runtime::Custom {
                            command: c.command,
                            flag: c.flag,
                        },
                    };

                    SubprocessMode::Script {
                        runtime,
                        body: script.body,
                        args: script.args,
                    }
                }
            };

            subprocess_mode
                .validate()
                .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

            Ok(TaskKind::Subprocess(SubprocessSpec {
                mode: subprocess_mode,
                env: convert_env(sub.env),
                cwd: sub.cwd.map(std::path::PathBuf::from),
                fail_on_non_zero: Flag::from(sub.fail_on_non_zero),
            }))
        }
        proto_api::task_kind::Kind::Wasm(wasm) => {
            if wasm.module.trim().is_empty() {
                return Err(ApiError::InvalidRequest("wasm module path is empty".into()));
            }

            Ok(TaskKind::Wasm(WasmSpec {
                module: std::path::PathBuf::from(wasm.module),
                args: wasm.args,
                env: convert_env(wasm.env),
            }))
        }
        proto_api::task_kind::Kind::Container(cont) => {
            if cont.image.trim().is_empty() {
                return Err(ApiError::InvalidRequest("container image is empty".into()));
            }

            Ok(TaskKind::Container(ContainerSpec {
                image: cont.image,
                command: if cont.command.is_empty() {
                    None
                } else {
                    Some(cont.command)
                },
                args: cont.args,
                env: convert_env(cont.env),
            }))
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

fn convert_restart_policy(
    strategy: proto_api::RestartStrategy,
    interval_ms: Option<u64>,
) -> Result<RestartPolicy, ApiError> {
    match strategy {
        proto_api::RestartStrategy::Never => Ok(RestartPolicy::Never),
        proto_api::RestartStrategy::OnFailure => Ok(RestartPolicy::OnFailure),
        proto_api::RestartStrategy::Always => Ok(RestartPolicy::Always { interval_ms }),

        proto_api::RestartStrategy::Unspecified => Err(ApiError::InvalidRequest(
            "restart strategy not specified".into(),
        )),
    }
}

fn convert_backoff_policy(backoff: proto_api::BackoffStrategy) -> Result<BackoffPolicy, ApiError> {
    let jitter = proto_api::JitterStrategy::try_from(backoff.jitter)
        .map_err(|_| ApiError::InvalidRequest("invalid jitter strategy".into()))?;

    let jitter = match jitter {
        proto_api::JitterStrategy::Decorrelated => JitterPolicy::Decorrelated,
        proto_api::JitterStrategy::Equal => JitterPolicy::Equal,
        proto_api::JitterStrategy::None => JitterPolicy::None,
        proto_api::JitterStrategy::Full => JitterPolicy::Full,

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
    if !backoff.factor.is_finite() || backoff.factor <= 0.0 {
        return Err(ApiError::InvalidRequest(
            "backoff factor must be a finite positive number".into(),
        ));
    }

    Ok(BackoffPolicy {
        jitter,
        first_ms: backoff.first_ms,
        max_ms: backoff.max_ms,
        factor: backoff.factor,
    })
}

fn convert_admission_policy(
    strategy: proto_api::AdmissionStrategy,
) -> Result<AdmissionPolicy, ApiError> {
    match strategy {
        proto_api::AdmissionStrategy::DropIfRunning => Ok(AdmissionPolicy::DropIfRunning),
        proto_api::AdmissionStrategy::Replace => Ok(AdmissionPolicy::Replace),
        proto_api::AdmissionStrategy::Queue => Ok(AdmissionPolicy::Queue),

        proto_api::AdmissionStrategy::Unspecified => Err(ApiError::InvalidRequest(
            "admission strategy not specified".into(),
        )),
    }
}

fn convert_labels(map: std::collections::HashMap<String, String>) -> Labels {
    let mut labels = Labels::new();
    for (k, v) in map {
        labels.insert(k, v);
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_subprocess_kind(command: &str) -> proto_api::TaskKind {
        proto_api::TaskKind {
            kind: Some(proto_api::task_kind::Kind::Subprocess(
                proto_api::SubprocessTask {
                    mode: Some(proto_api::subprocess_task::Mode::Command(
                        proto_api::CommandMode {
                            command: command.to_string(),
                            args: vec!["-l".to_string()],
                        },
                    )),
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
    fn create_spec_subprocess_valid() {
        let cs = convert_create_spec(make_valid_create_spec()).unwrap();
        assert_eq!(cs.slot(), "test-slot");
        assert_eq!(cs.timeout().as_millis(), 5_000);
        assert!(matches!(
            cs.kind(),
            TaskKind::Subprocess(SubprocessSpec { mode: SubprocessMode::Command { command, .. }, .. }) if command == "ls"
        ));
        assert!(matches!(cs.restart(), RestartPolicy::OnFailure));
        assert!(matches!(cs.admission(), AdmissionPolicy::DropIfRunning));
        assert_eq!(cs.backoff().first_ms, 100);
        assert_eq!(cs.backoff().max_ms, 10_000);
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

        let cs = convert_create_spec(spec).unwrap();
        assert!(
            matches!(cs.kind(), TaskKind::Wasm(WasmSpec { module, .. }) if module.to_str() == Some("/app/module.wasm"))
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

        let cs = convert_create_spec(spec).unwrap();
        assert!(
            matches!(cs.kind(), TaskKind::Container(ContainerSpec { image, .. }) if image == "alpine:latest")
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

        let cs = convert_create_spec(spec).unwrap();
        assert!(matches!(
            cs.kind(),
            TaskKind::Container(ContainerSpec { command: None, .. })
        ));
    }

    #[test]
    fn create_spec_always_with_interval() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Always as i32,
            restart_interval_ms: Some(5_000),
            ..make_valid_create_spec()
        };
        let cs = convert_create_spec(spec).unwrap();
        assert!(matches!(
            cs.restart(),
            RestartPolicy::Always {
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
        let cs = convert_create_spec(spec).unwrap();
        assert!(matches!(
            cs.restart(),
            RestartPolicy::Always { interval_ms: None }
        ));
    }

    #[test]
    fn create_spec_with_labels() {
        let mut labels = HashMap::new();
        labels.insert("runner-name".to_string(), "gpu".to_string());
        labels.insert("env".to_string(), "prod".to_string());

        let spec = proto_api::CreateSpec {
            labels,
            ..make_valid_create_spec()
        };

        let cs = convert_create_spec(spec).unwrap();
        assert_eq!(cs.labels().get("runner-name"), Some("gpu"));
        assert_eq!(cs.labels().get("env"), Some("prod"));
    }

    #[test]
    fn create_spec_env_conversion() {
        let cs = convert_create_spec(make_valid_create_spec()).unwrap();
        match cs.kind() {
            TaskKind::Subprocess(SubprocessSpec { env, .. }) => {
                assert_eq!(env.get("PATH"), Some("/usr/bin"));
            }
            _ => panic!("expected subprocess kind"),
        }
    }

    #[test]
    fn create_spec_subprocess_script_bash() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Subprocess(
                    proto_api::SubprocessTask {
                        mode: Some(proto_api::subprocess_task::Mode::Script(
                            proto_api::ScriptMode {
                                runtime: Some(proto_api::script_mode::Runtime::WellKnown(
                                    proto_api::ScriptRuntime::Bash as i32,
                                )),
                                body: BASE64.encode(b"echo hello"),
                                args: vec![],
                            },
                        )),
                        env: vec![],
                        cwd: None,
                        fail_on_non_zero: true,
                    },
                )),
            }),
            ..make_valid_create_spec()
        };

        let cs = convert_create_spec(spec).unwrap();
        match cs.kind() {
            TaskKind::Subprocess(SubprocessSpec { mode, .. }) => {
                assert!(matches!(
                    mode,
                    SubprocessMode::Script {
                        runtime: Runtime::Bash,
                        ..
                    }
                ));
            }
            _ => panic!("expected subprocess"),
        }
    }

    #[test]
    fn create_spec_subprocess_script_custom_runtime() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Subprocess(
                    proto_api::SubprocessTask {
                        mode: Some(proto_api::subprocess_task::Mode::Script(
                            proto_api::ScriptMode {
                                runtime: Some(proto_api::script_mode::Runtime::Custom(
                                    proto_api::CustomRuntime {
                                        command: "ruby".into(),
                                        flag: "-e".into(),
                                    },
                                )),
                                body: BASE64.encode(b"puts 'hello'"),
                                args: vec![],
                            },
                        )),
                        env: vec![],
                        cwd: None,
                        fail_on_non_zero: false,
                    },
                )),
            }),
            ..make_valid_create_spec()
        };

        let cs = convert_create_spec(spec).unwrap();
        match cs.kind() {
            TaskKind::Subprocess(SubprocessSpec { mode, .. }) => {
                assert!(matches!(
                    mode,
                    SubprocessMode::Script {
                        runtime: Runtime::Custom { .. },
                        ..
                    }
                ));
            }
            _ => panic!("expected subprocess"),
        }
    }

    #[test]
    fn restart_never() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Never as i32,
            ..make_valid_create_spec()
        };
        let cs = convert_create_spec(spec).unwrap();
        assert!(matches!(cs.restart(), RestartPolicy::Never));
    }

    #[test]
    fn all_jitter_policies_convert() {
        let cases = [
            (proto_api::JitterStrategy::None, JitterPolicy::None),
            (proto_api::JitterStrategy::Full, JitterPolicy::Full),
            (proto_api::JitterStrategy::Equal, JitterPolicy::Equal),
            (
                proto_api::JitterStrategy::Decorrelated,
                JitterPolicy::Decorrelated,
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
            let cs = convert_create_spec(spec).unwrap();
            assert_eq!(cs.backoff().jitter, expected);
        }
    }

    #[test]
    fn all_admission_policies_convert() {
        let cases = [
            (
                proto_api::AdmissionStrategy::DropIfRunning,
                AdmissionPolicy::DropIfRunning,
            ),
            (
                proto_api::AdmissionStrategy::Replace,
                AdmissionPolicy::Replace,
            ),
            (proto_api::AdmissionStrategy::Queue, AdmissionPolicy::Queue),
        ];

        for (proto_adm, expected) in cases {
            let spec = proto_api::CreateSpec {
                admission: proto_adm as i32,
                ..make_valid_create_spec()
            };
            let cs = convert_create_spec(spec).unwrap();
            assert_eq!(cs.admission(), expected);
        }
    }

    // ----- rejection paths -----

    #[test]
    fn reject_missing_kind() {
        let spec = proto_api::CreateSpec {
            kind: None,
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing task kind")));
    }

    #[test]
    fn reject_missing_kind_variant() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind { kind: None }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("command cannot be empty"))
        );
    }

    #[test]
    fn reject_whitespace_subprocess_command() {
        let spec = proto_api::CreateSpec {
            kind: Some(make_subprocess_kind("   ")),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("command cannot be empty"))
        );
    }

    #[test]
    fn reject_missing_subprocess_mode() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Subprocess(
                    proto_api::SubprocessTask {
                        mode: None,
                        env: vec![],
                        cwd: None,
                        fail_on_non_zero: false,
                    },
                )),
            }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing subprocess mode"))
        );
    }

    #[test]
    fn reject_empty_script_body() {
        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Subprocess(
                    proto_api::SubprocessTask {
                        mode: Some(proto_api::subprocess_task::Mode::Script(
                            proto_api::ScriptMode {
                                runtime: Some(proto_api::script_mode::Runtime::WellKnown(
                                    proto_api::ScriptRuntime::Bash as i32,
                                )),
                                body: "".into(),
                                args: vec![],
                            },
                        )),
                        env: vec![],
                        cwd: None,
                        fail_on_non_zero: false,
                    },
                )),
            }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("body cannot be empty"))
        );
    }

    #[test]
    fn reject_missing_script_runtime() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = proto_api::CreateSpec {
            kind: Some(proto_api::TaskKind {
                kind: Some(proto_api::task_kind::Kind::Subprocess(
                    proto_api::SubprocessTask {
                        mode: Some(proto_api::subprocess_task::Mode::Script(
                            proto_api::ScriptMode {
                                runtime: None,
                                body: BASE64.encode(b"echo hello"),
                                args: vec![],
                            },
                        )),
                        env: vec![],
                        cwd: None,
                        fail_on_non_zero: false,
                    },
                )),
            }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing script runtime"))
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
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
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("factor must be")));
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
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("factor must be")));
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
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("jitter")));
    }

    #[test]
    fn reject_unspecified_restart() {
        let spec = proto_api::CreateSpec {
            restart: proto_api::RestartStrategy::Unspecified as i32,
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("restart")));
    }

    #[test]
    fn reject_unspecified_admission() {
        let spec = proto_api::CreateSpec {
            admission: proto_api::AdmissionStrategy::Unspecified as i32,
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("admission")));
    }
}
