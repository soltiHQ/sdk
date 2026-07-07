//! # `TaskSpec` && `CreateSpec` conversion.
//!
//! This module covers both directions:
//!
//! | Direction     | Entry point              | Shape                                      |
//! |---------------|--------------------------|--------------------------------------------|
//! | Domain → wire | [`spec_to_proto`]        | used by `task.rs` when building `TaskData` |
//! | Wire → domain | [`convert_create_spec`]  | gRPC/HTTP submit path                      |

use std::num::NonZeroU32;

use solti_model::{
    AdmissionPolicy, BackoffPolicy, ContainerSpec, Flag, JitterPolicy, Labels, RestartPolicy,
    RunnerSelector, Runtime, SelectorOperator, SelectorRequirement, Slot, SubprocessMode,
    SubprocessSpec, TaskEnv, TaskKind, TaskSpec, WasmSpec,
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
        max_retries: spec.max_retries().map(NonZeroU32::get),
        runner_selector: spec.runner_selector().map(selector_to_proto),
    })
}

fn selector_to_proto(sel: &RunnerSelector) -> proto_api::RunnerSelector {
    proto_api::RunnerSelector {
        match_labels: sel
            .match_labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        match_expressions: sel
            .match_expressions
            .iter()
            .map(|req| proto_api::SelectorRequirement {
                key: req.key.clone(),
                operator: operator_to_proto(req.operator) as i32,
                values: req.values.clone(),
            })
            .collect(),
    }
}

fn operator_to_proto(op: SelectorOperator) -> proto_api::SelectorOperator {
    match op {
        SelectorOperator::In => proto_api::SelectorOperator::In,
        SelectorOperator::NotIn => proto_api::SelectorOperator::NotIn,
        SelectorOperator::Exists => proto_api::SelectorOperator::Exists,
        SelectorOperator::DoesNotExist => proto_api::SelectorOperator::DoesNotExist,
        _ => proto_api::SelectorOperator::Unspecified,
    }
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
                        _ => {
                            return Err(ApiError::Internal(
                                "unsupported script runtime variant".into(),
                            ));
                        }
                    };
                    proto_api::subprocess_task::Mode::Script(proto_api::ScriptMode {
                        runtime: Some(runtime_proto),
                        body: body.clone(),
                        args: args.clone(),
                    })
                }
                _ => {
                    return Err(ApiError::Internal(
                        "unsupported subprocess mode variant".into(),
                    ));
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

fn restart_to_proto(policy: RestartPolicy) -> (proto_api::RestartPolicy, Option<u64>) {
    match policy {
        RestartPolicy::Never => (proto_api::RestartPolicy::Never, None),
        RestartPolicy::OnFailure => (proto_api::RestartPolicy::OnFailure, None),
        RestartPolicy::Always { interval_ms } => (proto_api::RestartPolicy::Always, interval_ms),
        _ => (proto_api::RestartPolicy::Unspecified, None),
    }
}

fn backoff_to_proto(b: &BackoffPolicy) -> proto_api::BackoffPolicy {
    let jitter = match b.jitter {
        JitterPolicy::None => proto_api::JitterPolicy::None,
        JitterPolicy::Full => proto_api::JitterPolicy::Full,
        JitterPolicy::Equal => proto_api::JitterPolicy::Equal,
        JitterPolicy::Decorrelated => proto_api::JitterPolicy::Decorrelated,
        _ => proto_api::JitterPolicy::Unspecified,
    };
    proto_api::BackoffPolicy {
        jitter: jitter as i32,
        first_ms: b.first_ms,
        max_ms: b.max_ms,
        factor: b.factor,
    }
}

fn admission_to_proto(policy: AdmissionPolicy) -> proto_api::AdmissionPolicy {
    match policy {
        AdmissionPolicy::DropIfRunning => proto_api::AdmissionPolicy::DropIfRunning,
        AdmissionPolicy::Replace => proto_api::AdmissionPolicy::Replace,
        AdmissionPolicy::Queue => proto_api::AdmissionPolicy::Queue,
        _ => proto_api::AdmissionPolicy::Unspecified,
    }
}

/// Convert a proto [`proto_api::CreateSpec`] into a domain [`TaskSpec`].
///
/// Single validation gate for both transports: every submit/apply request passes through here.
///
/// ## Errors
///
/// - [`ApiError::InvalidRequest`]: the wire spec is not a valid [`TaskSpec`]. Causes:
///   - empty `slot`, `timeout_ms == 0`, or `max_retries == 0` (omit the field instead);
///   - missing `kind`, kind variant, subprocess mode, script runtime, or `backoff`;
///   - `UNSPECIFIED` / out-of-range enum value (restart, admission, jitter, selector operator);
///   - kind-specific field rejected (empty command, empty script body, empty wasm module path, empty container image);
///   - the final `TaskSpec::build` validation failed (e.g. backoff `factor < 1.0`).
pub fn convert_create_spec(spec: proto_api::CreateSpec) -> Result<TaskSpec, ApiError> {
    let slot: Slot = validate_slot(spec.slot)?.into();

    let kind = spec
        .kind
        .ok_or_else(|| ApiError::InvalidRequest("missing task kind".into()))?
        .kind
        .ok_or_else(|| ApiError::InvalidRequest("missing task kind variant".into()))?;

    let task_kind = convert_task_kind(kind)?;

    let restart = convert_restart_policy(
        proto_api::RestartPolicy::try_from(spec.restart)
            .map_err(|_| ApiError::InvalidRequest("invalid restart strategy".into()))?,
        spec.restart_interval_ms,
    )?;

    let backoff = spec
        .backoff
        .ok_or_else(|| ApiError::InvalidRequest("missing backoff strategy".into()))?;

    let max_retries = match spec.max_retries {
        None => None,
        Some(0) => {
            return Err(ApiError::InvalidRequest(
                "max_retries: 0 is not allowed; omit the field for an unlimited budget".into(),
            ));
        }
        Some(n) => NonZeroU32::new(n),
    };

    let mut builder = TaskSpec::builder(slot, task_kind, validate_timeout(spec.timeout_ms)?)
        .restart(restart)
        .backoff(convert_backoff_policy(backoff)?)
        .admission(convert_admission_policy(
            proto_api::AdmissionPolicy::try_from(spec.admission)
                .map_err(|_| ApiError::InvalidRequest("invalid admission strategy".into()))?,
        )?)
        .labels(convert_labels(spec.labels))
        .max_retries(max_retries);

    if let Some(sel) = spec.runner_selector {
        builder = builder.runner_selector(convert_runner_selector(sel)?);
    }

    let task_spec = builder
        .build()
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    Ok(task_spec)
}

fn convert_runner_selector(sel: proto_api::RunnerSelector) -> Result<RunnerSelector, ApiError> {
    let match_expressions = sel
        .match_expressions
        .into_iter()
        .map(|req| {
            let operator = match proto_api::SelectorOperator::try_from(req.operator) {
                Ok(proto_api::SelectorOperator::In) => SelectorOperator::In,
                Ok(proto_api::SelectorOperator::NotIn) => SelectorOperator::NotIn,
                Ok(proto_api::SelectorOperator::Exists) => SelectorOperator::Exists,
                Ok(proto_api::SelectorOperator::DoesNotExist) => SelectorOperator::DoesNotExist,
                _ => {
                    return Err(ApiError::InvalidRequest(format!(
                        "invalid selector operator for key '{}'",
                        req.key
                    )));
                }
            };
            Ok(SelectorRequirement {
                key: req.key,
                operator,
                values: req.values,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;

    Ok(RunnerSelector {
        match_labels: convert_labels(sel.match_labels),
        match_expressions,
    })
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

            Ok(TaskKind::Subprocess(SubprocessSpec::new(
                subprocess_mode,
                convert_env(sub.env),
                sub.cwd.map(std::path::PathBuf::from),
                Flag::from(sub.fail_on_non_zero),
            )))
        }
        proto_api::task_kind::Kind::Wasm(wasm) => {
            if wasm.module.trim().is_empty() {
                return Err(ApiError::InvalidRequest("wasm module path is empty".into()));
            }

            Ok(TaskKind::Wasm(WasmSpec::new(
                std::path::PathBuf::from(wasm.module),
                wasm.args,
                convert_env(wasm.env),
            )))
        }
        proto_api::task_kind::Kind::Container(cont) => {
            if cont.image.trim().is_empty() {
                return Err(ApiError::InvalidRequest("container image is empty".into()));
            }

            Ok(TaskKind::Container(ContainerSpec::new(
                cont.image,
                if cont.command.is_empty() {
                    None
                } else {
                    Some(cont.command)
                },
                cont.args,
                convert_env(cont.env),
            )))
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
    strategy: proto_api::RestartPolicy,
    interval_ms: Option<u64>,
) -> Result<RestartPolicy, ApiError> {
    match strategy {
        proto_api::RestartPolicy::Never => Ok(RestartPolicy::Never),
        proto_api::RestartPolicy::OnFailure => Ok(RestartPolicy::OnFailure),
        proto_api::RestartPolicy::Always => Ok(RestartPolicy::Always { interval_ms }),

        proto_api::RestartPolicy::Unspecified => Err(ApiError::InvalidRequest(
            "restart strategy not specified".into(),
        )),
    }
}

fn convert_backoff_policy(backoff: proto_api::BackoffPolicy) -> Result<BackoffPolicy, ApiError> {
    let jitter = proto_api::JitterPolicy::try_from(backoff.jitter)
        .map_err(|_| ApiError::InvalidRequest("invalid jitter strategy".into()))?;

    let jitter = match jitter {
        proto_api::JitterPolicy::Decorrelated => JitterPolicy::Decorrelated,
        proto_api::JitterPolicy::Equal => JitterPolicy::Equal,
        proto_api::JitterPolicy::None => JitterPolicy::None,
        proto_api::JitterPolicy::Full => JitterPolicy::Full,

        proto_api::JitterPolicy::Unspecified => {
            return Err(ApiError::InvalidRequest(
                "jitter strategy not specified".into(),
            ));
        }
    };

    // No business validation here: the model's BackoffPolicy::validate is the
    // single source of the rules and runs inside TaskSpec::build. Duplicating
    // it once produced drifted rules (the API accepted factor < 1.0 that the
    // model rejected later).
    Ok(BackoffPolicy {
        jitter,
        first_ms: backoff.first_ms,
        max_ms: backoff.max_ms,
        factor: backoff.factor,
    })
}

fn convert_admission_policy(
    strategy: proto_api::AdmissionPolicy,
) -> Result<AdmissionPolicy, ApiError> {
    match strategy {
        proto_api::AdmissionPolicy::DropIfRunning => Ok(AdmissionPolicy::DropIfRunning),
        proto_api::AdmissionPolicy::Replace => Ok(AdmissionPolicy::Replace),
        proto_api::AdmissionPolicy::Queue => Ok(AdmissionPolicy::Queue),

        proto_api::AdmissionPolicy::Unspecified => Err(ApiError::InvalidRequest(
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

    fn make_backoff() -> proto_api::BackoffPolicy {
        proto_api::BackoffPolicy {
            jitter: proto_api::JitterPolicy::Full as i32,
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
            restart: proto_api::RestartPolicy::OnFailure as i32,
            restart_interval_ms: None,
            backoff: Some(make_backoff()),
            admission: proto_api::AdmissionPolicy::DropIfRunning as i32,
            labels: HashMap::new(),
            max_retries: None,
            runner_selector: None,
        }
    }

    #[test]
    fn create_spec_max_retries_round_trips() {
        let mut proto = make_valid_create_spec();
        proto.max_retries = Some(3);

        let spec = convert_create_spec(proto).unwrap();
        assert_eq!(spec.max_retries().map(NonZeroU32::get), Some(3));

        let back = spec_to_proto(&spec).unwrap();
        assert_eq!(back.max_retries, Some(3));
    }

    #[test]
    fn create_spec_zero_max_retries_is_rejected() {
        let mut proto = make_valid_create_spec();
        proto.max_retries = Some(0);

        let err = convert_create_spec(proto).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("max_retries")));
    }

    #[test]
    fn create_spec_runner_selector_round_trips() {
        let mut proto = make_valid_create_spec();
        proto.runner_selector = Some(proto_api::RunnerSelector {
            match_labels: HashMap::from([("zone".to_string(), "eu".to_string())]),
            match_expressions: vec![proto_api::SelectorRequirement {
                key: "arch".to_string(),
                operator: proto_api::SelectorOperator::In as i32,
                values: vec!["arm64".to_string()],
            }],
        });

        let spec = convert_create_spec(proto).unwrap();
        let sel = spec
            .runner_selector()
            .expect("selector must survive convert");
        assert_eq!(sel.match_expressions.len(), 1);
        assert_eq!(sel.match_expressions[0].operator, SelectorOperator::In);
        assert_eq!(sel.match_expressions[0].values, vec!["arm64".to_string()]);

        let back = spec_to_proto(&spec).unwrap();
        let back_sel = back
            .runner_selector
            .expect("selector must survive round-trip");
        assert_eq!(
            back_sel.match_labels.get("zone").map(String::as_str),
            Some("eu")
        );
        assert_eq!(back_sel.match_expressions.len(), 1);
    }

    #[test]
    fn create_spec_invalid_selector_operator_is_rejected() {
        let mut proto = make_valid_create_spec();
        proto.runner_selector = Some(proto_api::RunnerSelector {
            match_labels: HashMap::new(),
            match_expressions: vec![proto_api::SelectorRequirement {
                key: "arch".to_string(),
                operator: 999,
                values: vec![],
            }],
        });

        let err = convert_create_spec(proto).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("arch")));
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
            restart: proto_api::RestartPolicy::Always as i32,
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
            restart: proto_api::RestartPolicy::Always as i32,
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
            restart: proto_api::RestartPolicy::Never as i32,
            ..make_valid_create_spec()
        };
        let cs = convert_create_spec(spec).unwrap();
        assert!(matches!(cs.restart(), RestartPolicy::Never));
    }

    #[test]
    fn all_jitter_policies_convert() {
        let cases = [
            (proto_api::JitterPolicy::None, JitterPolicy::None),
            (proto_api::JitterPolicy::Full, JitterPolicy::Full),
            (proto_api::JitterPolicy::Equal, JitterPolicy::Equal),
            (
                proto_api::JitterPolicy::Decorrelated,
                JitterPolicy::Decorrelated,
            ),
        ];

        for (proto_jitter, expected) in cases {
            let spec = proto_api::CreateSpec {
                backoff: Some(proto_api::BackoffPolicy {
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
                proto_api::AdmissionPolicy::DropIfRunning,
                AdmissionPolicy::DropIfRunning,
            ),
            (
                proto_api::AdmissionPolicy::Replace,
                AdmissionPolicy::Replace,
            ),
            (proto_api::AdmissionPolicy::Queue, AdmissionPolicy::Queue),
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
            backoff: Some(proto_api::BackoffPolicy {
                first_ms: 0,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("first_ms must be greater than zero"))
        );
    }

    #[test]
    fn reject_zero_backoff_max_ms() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffPolicy {
                max_ms: 0,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("max_ms must be >= first_ms"))
        );
    }

    #[test]
    fn reject_sub_one_backoff_factor() {
        // Regression: factor 0.5 used to pass the API precheck (factor > 0.0)
        // and fail later inside build with a confusing error. The model rule
        // (factor >= 1.0) is now the only rule.
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffPolicy {
                factor: 0.5,
                ..make_backoff()
            }),
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains(">= 1.0")));
    }

    #[test]
    fn reject_negative_backoff_factor() {
        let spec = proto_api::CreateSpec {
            backoff: Some(proto_api::BackoffPolicy {
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
            backoff: Some(proto_api::BackoffPolicy {
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
            backoff: Some(proto_api::BackoffPolicy {
                jitter: proto_api::JitterPolicy::Unspecified as i32,
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
            restart: proto_api::RestartPolicy::Unspecified as i32,
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("restart")));
    }

    #[test]
    fn reject_unspecified_admission() {
        let spec = proto_api::CreateSpec {
            admission: proto_api::AdmissionPolicy::Unspecified as i32,
            ..make_valid_create_spec()
        };
        let err = convert_create_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("admission")));
    }
}
