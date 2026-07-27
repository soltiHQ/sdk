//! # Domain/wire `TaskSpec` conversion.
//!
//! This module covers both directions:
//!
//! | Direction     | Entry point              | Shape                                      |
//! |---------------|--------------------------|--------------------------------------------|
//! | Domain → wire | [`spec_to_proto`]       | resource responses                         |
//! | Wire → domain | [`convert_task_spec`]   | create/apply manifests                     |

use std::num::NonZeroU32;

use solti_model::{Slot, TaskSpec};

use crate::error::ApiError;
use crate::proto_api;
use crate::validate::{validate_slot, validate_timeout};

use super::policy::{
    admission_to_proto, backoff_to_proto, convert_admission_policy, convert_backoff_policy,
    convert_restart_policy, restart_to_proto,
};
pub(super) use super::selector::convert_labels;
use super::selector::{convert_label_selector, selector_to_proto};
use super::workload::{convert_task_workload, workload_to_proto};

/// Build a wire [`proto_api::TaskSpec`] from a domain [`TaskSpec`].
pub(super) fn spec_to_proto(spec: &TaskSpec) -> Result<proto_api::TaskSpec, ApiError> {
    let (restart, restart_interval_ms) = restart_to_proto(spec.restart())?;
    Ok(proto_api::TaskSpec {
        admission: admission_to_proto(spec.admission())? as i32,
        backoff: Some(backoff_to_proto(spec.backoff())?),
        workload: Some(workload_to_proto(spec.workload())?),
        timeout_ms: spec.timeout().as_millis(),
        slot: spec.slot().to_string(),
        restart: restart as i32,
        restart_interval_ms,
        max_retries: spec.max_retries().map(NonZeroU32::get),
        runner_selector: spec.runner_selector().map(selector_to_proto).transpose()?,
    })
}

/// Convert a wire [`proto_api::TaskSpec`] into a domain [`TaskSpec`].
///
/// Single validation gate for both transports: every submit/apply request passes through here.
///
/// ## Errors
///
/// - [`ApiError::InvalidRequest`]: the wire spec is not a valid [`TaskSpec`]. Causes:
///   - empty `slot`, `timeout_ms == 0`, or `max_retries == 0` (omit the field instead);
///   - missing `kind`, kind variant, subprocess mode, or `backoff`;
///   - `UNSPECIFIED` / out-of-range enum value (restart, admission, jitter, selector operator);
///   - kind-specific field rejected (empty command, interpreter, script body, wasm module path, or container image);
///   - the final `TaskSpec::build` validation failed (e.g. backoff `factor < 1.0`).
pub fn convert_task_spec(spec: proto_api::TaskSpec) -> Result<TaskSpec, ApiError> {
    let slot: Slot = validate_slot(spec.slot)?;

    let workload = spec
        .workload
        .ok_or_else(|| ApiError::InvalidRequest("missing task workload".into()))?;
    let task_workload = convert_task_workload(workload)?;

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

    let mut builder = TaskSpec::builder(slot, task_workload, validate_timeout(spec.timeout_ms)?)
        .restart(restart)
        .backoff(convert_backoff_policy(backoff)?)
        .admission(convert_admission_policy(
            proto_api::AdmissionPolicy::try_from(spec.admission)
                .map_err(|_| ApiError::InvalidRequest("invalid admission strategy".into()))?,
        )?)
        .max_retries(max_retries);

    if let Some(sel) = spec.runner_selector {
        builder = builder.runner_selector(convert_label_selector(sel)?);
    }

    let task_spec = builder
        .build()
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    Ok(task_spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::{
        AdmissionPolicy, ContainerSpec, ExtensionWorkload, JitterPolicy, RestartPolicy,
        SelectorOperator, SubprocessMode, SubprocessSpec, TaskWorkload, WORKLOAD_API_VERSION,
        WasmSpec,
    };
    use std::collections::HashMap;

    fn make_workload(kind: &str, spec: proto_api::task_workload::Spec) -> proto_api::TaskWorkload {
        proto_api::TaskWorkload {
            api_version: WORKLOAD_API_VERSION.to_owned(),
            kind: kind.to_owned(),
            spec: Some(spec),
        }
    }

    fn make_subprocess_workload(command: &str) -> proto_api::TaskWorkload {
        make_workload(
            "Subprocess",
            proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                mode: Some(proto_api::SubprocessMode {
                    mode: Some(proto_api::subprocess_mode::Mode::Command(
                        proto_api::CommandMode {
                            command: command.to_string(),
                            args: vec!["-l".to_string()],
                        },
                    )),
                }),
                env: vec![proto_api::KeyValue {
                    key: "PATH".to_string(),
                    value: "/usr/bin".to_string(),
                }],
                cwd: Some("/tmp".to_string()),
                fail_on_non_zero: true,
            }),
        )
    }

    fn make_backoff() -> proto_api::BackoffPolicy {
        proto_api::BackoffPolicy {
            jitter: proto_api::JitterPolicy::Full as i32,
            first_ms: 100,
            max_ms: 10_000,
            factor: 2.0,
        }
    }

    fn make_valid_task_spec() -> proto_api::TaskSpec {
        proto_api::TaskSpec {
            slot: "test-slot".to_string(),
            workload: Some(make_subprocess_workload("ls")),
            timeout_ms: 5_000,
            restart: proto_api::RestartPolicy::OnFailure as i32,
            restart_interval_ms: None,
            backoff: Some(make_backoff()),
            admission: proto_api::AdmissionPolicy::DropIfRunning as i32,
            max_retries: None,
            runner_selector: None,
        }
    }

    #[test]
    fn task_spec_max_retries_round_trips() {
        let mut proto = make_valid_task_spec();
        proto.max_retries = Some(3);

        let spec = convert_task_spec(proto).unwrap();
        assert_eq!(spec.max_retries().map(NonZeroU32::get), Some(3));

        let back = spec_to_proto(&spec).unwrap();
        assert_eq!(back.max_retries, Some(3));
    }

    #[test]
    fn task_spec_zero_max_retries_is_rejected() {
        let mut proto = make_valid_task_spec();
        proto.max_retries = Some(0);

        let err = convert_task_spec(proto).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("max_retries")));
    }

    #[test]
    fn task_spec_runner_selector_round_trips() {
        let mut proto = make_valid_task_spec();
        proto.runner_selector = Some(proto_api::LabelSelector {
            match_labels: HashMap::from([("zone".to_string(), "eu".to_string())]),
            match_expressions: vec![proto_api::SelectorRequirement {
                key: "arch".to_string(),
                operator: proto_api::SelectorOperator::In as i32,
                values: vec!["arm64".to_string()],
            }],
        });

        let spec = convert_task_spec(proto).unwrap();
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
    fn task_spec_invalid_selector_operator_is_rejected() {
        let mut proto = make_valid_task_spec();
        proto.runner_selector = Some(proto_api::LabelSelector {
            match_labels: HashMap::new(),
            match_expressions: vec![proto_api::SelectorRequirement {
                key: "arch".to_string(),
                operator: 999,
                values: vec![],
            }],
        });

        let err = convert_task_spec(proto).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("arch")));
    }

    #[test]
    fn task_spec_subprocess_valid() {
        let cs = convert_task_spec(make_valid_task_spec()).unwrap();
        assert_eq!(cs.slot(), "test-slot");
        assert_eq!(cs.timeout().as_millis(), 5_000);
        assert!(matches!(
            cs.workload(),
            TaskWorkload::Subprocess(SubprocessSpec { mode: SubprocessMode::Command { command, .. }, .. }) if command == "ls"
        ));
        assert!(matches!(cs.restart(), RestartPolicy::OnFailure));
        assert!(matches!(cs.admission(), AdmissionPolicy::DropIfRunning));
        assert_eq!(cs.backoff().first_ms, 100);
        assert_eq!(cs.backoff().max_ms, 10_000);
    }

    #[test]
    fn task_spec_wasm_valid() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Wasm",
                proto_api::task_workload::Spec::Wasm(proto_api::WasmTask {
                    module: "/app/module.wasm".to_string(),
                    args: vec!["--verbose".to_string()],
                    env: vec![],
                }),
            )),
            ..make_valid_task_spec()
        };

        let cs = convert_task_spec(spec).unwrap();
        assert!(
            matches!(cs.workload(), TaskWorkload::Wasm(WasmSpec { module, .. }) if module.to_str() == Some("/app/module.wasm"))
        );
    }

    #[test]
    fn task_spec_container_valid() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Container",
                proto_api::task_workload::Spec::Container(proto_api::ContainerTask {
                    image: "alpine:latest".to_string(),
                    command: Some(proto_api::ContainerCommand {
                        items: vec!["sh".to_string(), "-c".to_string()],
                    }),
                    args: vec!["echo hello".to_string()],
                    env: vec![],
                }),
            )),
            ..make_valid_task_spec()
        };

        let cs = convert_task_spec(spec).unwrap();
        assert!(
            matches!(cs.workload(), TaskWorkload::Container(ContainerSpec { image, .. }) if image == "alpine:latest")
        );
    }

    #[test]
    fn task_spec_container_absent_command_becomes_none() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Container",
                proto_api::task_workload::Spec::Container(proto_api::ContainerTask {
                    image: "nginx".to_string(),
                    command: None,
                    args: vec![],
                    env: vec![],
                }),
            )),
            ..make_valid_task_spec()
        };

        let cs = convert_task_spec(spec).unwrap();
        assert!(matches!(
            cs.workload(),
            TaskWorkload::Container(ContainerSpec { command: None, .. })
        ));
    }

    #[test]
    fn task_spec_container_present_empty_command_stays_some_empty() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Container",
                proto_api::task_workload::Spec::Container(proto_api::ContainerTask {
                    image: "nginx".to_string(),
                    command: Some(proto_api::ContainerCommand { items: vec![] }),
                    args: vec![],
                    env: vec![],
                }),
            )),
            ..make_valid_task_spec()
        };

        let converted = convert_task_spec(spec).unwrap();
        assert!(matches!(
            converted.workload(),
            TaskWorkload::Container(ContainerSpec { command: Some(command), .. }) if command.is_empty()
        ));

        let proto = spec_to_proto(&converted).unwrap();
        let Some(proto_api::task_workload::Spec::Container(container)) =
            proto.workload.and_then(|workload| workload.spec)
        else {
            panic!("expected container workload");
        };
        assert_eq!(container.command.unwrap().items, Vec::<String>::new());
    }

    #[test]
    fn extension_workload_round_trips_nested_json_object() {
        let expected = serde_json::json!({
            "enabled": true,
            "exactInteger": 9_007_199_254_740_993_u64,
            "limits": { "cpu": 2.5, "memory": null },
            "names": ["primary", "replica"]
        });
        let workload = TaskWorkload::Extension(
            ExtensionWorkload::new(
                "workloads.example.io/v1",
                "DatabaseBackup",
                expected.clone(),
            )
            .unwrap(),
        );

        let proto = workload_to_proto(&workload).unwrap();
        assert_eq!(proto.api_version, "workloads.example.io/v1");
        assert_eq!(proto.kind, "DatabaseBackup");

        let converted = convert_task_workload(proto).unwrap();
        let TaskWorkload::Extension(extension) = converted else {
            panic!("expected extension workload");
        };
        assert_eq!(extension.api_version(), "workloads.example.io/v1");
        assert_eq!(extension.kind(), "DatabaseBackup");
        assert_eq!(extension.spec(), &expected);
    }

    #[test]
    fn extension_workload_uses_model_reserved_gvk_validation() {
        let proto = proto_api::TaskWorkload {
            api_version: WORKLOAD_API_VERSION.to_owned(),
            kind: "Subprocess".into(),
            spec: Some(proto_api::task_workload::Spec::Extension(
                proto_api::ExtensionTask {
                    spec: Some(proto_api::RawExtension {
                        raw: b"{}".to_vec(),
                    }),
                },
            )),
        };

        let error = convert_task_workload(proto).unwrap_err();
        assert!(matches!(error, ApiError::InvalidRequest(message) if message.contains("reserved")));
    }

    #[test]
    fn extension_workload_rejects_missing_struct() {
        let proto = proto_api::TaskWorkload {
            api_version: "workloads.example.io/v1".into(),
            kind: "DatabaseBackup".into(),
            spec: Some(proto_api::task_workload::Spec::Extension(
                proto_api::ExtensionTask { spec: None },
            )),
        };

        let error = convert_task_workload(proto).unwrap_err();
        assert!(
            matches!(error, ApiError::InvalidRequest(message) if message.contains("missing extension workload spec"))
        );
    }

    #[test]
    fn extension_workload_rejects_non_object_raw_json() {
        let proto = proto_api::TaskWorkload {
            api_version: "workloads.example.io/v1".into(),
            kind: "DatabaseBackup".into(),
            spec: Some(proto_api::task_workload::Spec::Extension(
                proto_api::ExtensionTask {
                    spec: Some(proto_api::RawExtension {
                        raw: br#"["not", "an", "object"]"#.to_vec(),
                    }),
                },
            )),
        };

        let error = convert_task_workload(proto).unwrap_err();
        assert!(
            matches!(error, ApiError::InvalidRequest(message) if message.contains("JSON object"))
        );
    }

    #[test]
    fn task_spec_always_with_interval() {
        let spec = proto_api::TaskSpec {
            restart: proto_api::RestartPolicy::Always as i32,
            restart_interval_ms: Some(5_000),
            ..make_valid_task_spec()
        };
        let cs = convert_task_spec(spec).unwrap();
        assert!(matches!(
            cs.restart(),
            RestartPolicy::Always {
                interval_ms: Some(5_000)
            }
        ));
    }

    #[test]
    fn task_spec_always_without_interval() {
        let spec = proto_api::TaskSpec {
            restart: proto_api::RestartPolicy::Always as i32,
            restart_interval_ms: None,
            ..make_valid_task_spec()
        };
        let cs = convert_task_spec(spec).unwrap();
        assert!(matches!(
            cs.restart(),
            RestartPolicy::Always { interval_ms: None }
        ));
    }

    #[test]
    fn metadata_labels_convert() {
        let mut labels = HashMap::new();
        labels.insert("runner-name".to_string(), "gpu".to_string());
        labels.insert("env".to_string(), "prod".to_string());

        let labels = convert_labels(labels);
        assert_eq!(labels.get("runner-name"), Some("gpu"));
        assert_eq!(labels.get("env"), Some("prod"));
    }

    #[test]
    fn task_spec_env_conversion() {
        let cs = convert_task_spec(make_valid_task_spec()).unwrap();
        match cs.workload() {
            TaskWorkload::Subprocess(SubprocessSpec { env, .. }) => {
                assert_eq!(env.get("PATH"), Some("/usr/bin"));
            }
            _ => panic!("expected subprocess kind"),
        }
    }

    #[test]
    fn task_spec_subprocess_script_interpreter() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Subprocess",
                proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                    mode: Some(proto_api::SubprocessMode {
                        mode: Some(proto_api::subprocess_mode::Mode::Script(
                            proto_api::ScriptMode {
                                interpreter: "bash".into(),
                                body: BASE64.encode(b"echo hello"),
                                args: vec![],
                            },
                        )),
                    }),
                    env: vec![],
                    cwd: None,
                    fail_on_non_zero: true,
                }),
            )),
            ..make_valid_task_spec()
        };

        let cs = convert_task_spec(spec).unwrap();
        match cs.workload() {
            TaskWorkload::Subprocess(SubprocessSpec { mode, .. }) => {
                assert!(matches!(
                    mode,
                    SubprocessMode::Script {
                        interpreter,
                        ..
                    } if interpreter == "bash"
                ));
            }
            _ => panic!("expected subprocess"),
        }
    }

    #[test]
    fn task_spec_subprocess_script_custom_interpreter() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Subprocess",
                proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                    mode: Some(proto_api::SubprocessMode {
                        mode: Some(proto_api::subprocess_mode::Mode::Script(
                            proto_api::ScriptMode {
                                interpreter: "ruby".into(),
                                body: BASE64.encode(b"puts 'hello'"),
                                args: vec![],
                            },
                        )),
                    }),
                    env: vec![],
                    cwd: None,
                    fail_on_non_zero: false,
                }),
            )),
            ..make_valid_task_spec()
        };

        let cs = convert_task_spec(spec).unwrap();
        match cs.workload() {
            TaskWorkload::Subprocess(SubprocessSpec { mode, .. }) => {
                assert!(matches!(
                    mode,
                    SubprocessMode::Script {
                        interpreter,
                        ..
                    } if interpreter == "ruby"
                ));
            }
            _ => panic!("expected subprocess"),
        }
    }

    #[test]
    fn restart_never() {
        let spec = proto_api::TaskSpec {
            restart: proto_api::RestartPolicy::Never as i32,
            ..make_valid_task_spec()
        };
        let cs = convert_task_spec(spec).unwrap();
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
            let spec = proto_api::TaskSpec {
                backoff: Some(proto_api::BackoffPolicy {
                    jitter: proto_jitter as i32,
                    ..make_backoff()
                }),
                ..make_valid_task_spec()
            };
            let cs = convert_task_spec(spec).unwrap();
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
            let spec = proto_api::TaskSpec {
                admission: proto_adm as i32,
                ..make_valid_task_spec()
            };
            let cs = convert_task_spec(spec).unwrap();
            assert_eq!(cs.admission(), expected);
        }
    }

    // ----- rejection paths -----

    #[test]
    fn reject_missing_workload() {
        let spec = proto_api::TaskSpec {
            workload: None,
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing task workload"))
        );
    }

    #[test]
    fn reject_missing_workload_spec() {
        let spec = proto_api::TaskSpec {
            workload: Some(proto_api::TaskWorkload {
                api_version: WORKLOAD_API_VERSION.to_owned(),
                kind: "Subprocess".into(),
                spec: None,
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing workload spec"))
        );
    }

    #[test]
    fn reject_unknown_workload_api_version() {
        let mut workload = make_subprocess_workload("echo");
        workload.api_version = "other.io/v1".into();
        let spec = proto_api::TaskSpec {
            workload: Some(workload),
            ..make_valid_task_spec()
        };

        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("apiVersion")));
    }

    #[test]
    fn reject_workload_kind_that_disagrees_with_spec() {
        let mut workload = make_subprocess_workload("echo");
        workload.kind = "Container".into();
        let spec = proto_api::TaskSpec {
            workload: Some(workload),
            ..make_valid_task_spec()
        };

        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("does not match")));
    }

    #[test]
    fn reject_empty_subprocess_command() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_subprocess_workload("")),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("command cannot be empty"))
        );
    }

    #[test]
    fn reject_whitespace_subprocess_command() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_subprocess_workload("   ")),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("command cannot be empty"))
        );
    }

    #[test]
    fn reject_missing_subprocess_mode() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Subprocess",
                proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                    mode: None,
                    env: vec![],
                    cwd: None,
                    fail_on_non_zero: false,
                }),
            )),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing subprocess mode"))
        );
    }

    #[test]
    fn reject_empty_script_body() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Subprocess",
                proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                    mode: Some(proto_api::SubprocessMode {
                        mode: Some(proto_api::subprocess_mode::Mode::Script(
                            proto_api::ScriptMode {
                                interpreter: "bash".into(),
                                body: "".into(),
                                args: vec![],
                            },
                        )),
                    }),
                    env: vec![],
                    cwd: None,
                    fail_on_non_zero: false,
                }),
            )),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("body cannot be empty"))
        );
    }

    #[test]
    fn reject_empty_script_interpreter() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Subprocess",
                proto_api::task_workload::Spec::Subprocess(proto_api::SubprocessTask {
                    mode: Some(proto_api::SubprocessMode {
                        mode: Some(proto_api::subprocess_mode::Mode::Script(
                            proto_api::ScriptMode {
                                interpreter: "".into(),
                                body: BASE64.encode(b"echo hello"),
                                args: vec![],
                            },
                        )),
                    }),
                    env: vec![],
                    cwd: None,
                    fail_on_non_zero: false,
                }),
            )),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("interpreter cannot be empty"))
        );
    }

    #[test]
    fn reject_empty_wasm_module() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Wasm",
                proto_api::task_workload::Spec::Wasm(proto_api::WasmTask {
                    module: "".to_string(),
                    args: vec![],
                    env: vec![],
                }),
            )),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("wasm module path is empty"))
        );
    }

    #[test]
    fn reject_empty_container_image() {
        let spec = proto_api::TaskSpec {
            workload: Some(make_workload(
                "Container",
                proto_api::task_workload::Spec::Container(proto_api::ContainerTask {
                    image: "".to_string(),
                    command: None,
                    args: vec![],
                    env: vec![],
                }),
            )),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("container image is empty"))
        );
    }

    #[test]
    fn reject_empty_slot() {
        let spec = proto_api::TaskSpec {
            slot: "".to_string(),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("invalid slot")));
    }

    #[test]
    fn reject_whitespace_slot() {
        let spec = proto_api::TaskSpec {
            slot: "   ".to_string(),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("invalid slot")));
    }

    #[test]
    fn reject_zero_timeout() {
        let spec = proto_api::TaskSpec {
            timeout_ms: 0,
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("timeout_ms cannot be zero"))
        );
    }

    #[test]
    fn reject_missing_backoff() {
        let spec = proto_api::TaskSpec {
            backoff: None,
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("missing backoff")));
    }

    #[test]
    fn reject_zero_backoff_first_ms() {
        let spec = proto_api::TaskSpec {
            backoff: Some(proto_api::BackoffPolicy {
                first_ms: 0,
                ..make_backoff()
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("first_ms must be greater than zero"))
        );
    }

    #[test]
    fn reject_zero_backoff_max_ms() {
        let spec = proto_api::TaskSpec {
            backoff: Some(proto_api::BackoffPolicy {
                max_ms: 0,
                ..make_backoff()
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidRequest(msg) if msg.contains("max_ms must be >= first_ms"))
        );
    }

    #[test]
    fn reject_sub_one_backoff_factor() {
        // Regression: factor 0.5 used to pass the API precheck (factor > 0.0)
        // and fail later inside build with a confusing error. The model rule
        // (factor >= 1.0) is now the only rule.
        let spec = proto_api::TaskSpec {
            backoff: Some(proto_api::BackoffPolicy {
                factor: 0.5,
                ..make_backoff()
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains(">= 1.0")));
    }

    #[test]
    fn reject_negative_backoff_factor() {
        let spec = proto_api::TaskSpec {
            backoff: Some(proto_api::BackoffPolicy {
                factor: -1.0,
                ..make_backoff()
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("factor must be")));
    }

    #[test]
    fn reject_zero_backoff_factor() {
        let spec = proto_api::TaskSpec {
            backoff: Some(proto_api::BackoffPolicy {
                factor: 0.0,
                ..make_backoff()
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("factor must be")));
    }

    #[test]
    fn reject_unspecified_jitter() {
        let spec = proto_api::TaskSpec {
            backoff: Some(proto_api::BackoffPolicy {
                jitter: proto_api::JitterPolicy::Unspecified as i32,
                ..make_backoff()
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("jitter")));
    }

    #[test]
    fn reject_unspecified_restart() {
        let spec = proto_api::TaskSpec {
            restart: proto_api::RestartPolicy::Unspecified as i32,
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("restart")));
    }

    #[test]
    fn reject_unspecified_admission() {
        let spec = proto_api::TaskSpec {
            admission: proto_api::AdmissionPolicy::Unspecified as i32,
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("admission")));
    }
}
