//! # Task Spec Conversion
//!
//! Converts task specs for the gRPC transport.
//!
//! | Direction     | Entry point            | Use                    |
//! |---------------|------------------------|------------------------|
//! | Domain → wire | [`spec_to_proto`]      | Resource responses     |
//! | Wire → domain | [`convert_task_spec`]  | Create and apply input |

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

/// Converts a domain task spec into protobuf.
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

/// Converts a protobuf task spec into the domain model.
///
/// HTTP does not use this function.
/// HTTP deserializes the model-owned CRD representation directly.
///
/// ## Errors
///
/// Returns [`ApiError::InvalidRequest`] when:
///
/// - The slot or timeout is invalid.
/// - The workload or backoff is absent.
/// - `max_retries` is zero.
/// - An enum or selector value is invalid.
/// - Workload validation fails.
/// - The final [`TaskSpec`] validation fails.
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
    use solti_chain::{CHAIN_API_VERSION, CHAIN_KIND, ChainSpec, ChainStep, FailureMode};
    use solti_model::{
        AdmissionPolicy, JitterPolicy, RestartPolicy, SelectorOperator, TaskWorkload, Workload,
    };
    use std::collections::HashMap;

    fn make_workload(command: &str) -> proto_api::TaskWorkload {
        proto_api::TaskWorkload {
            api_version: "tasks.example.io/v1".to_owned(),
            kind: "Echo".to_owned(),
            spec: Some(proto_api::task_workload::Spec::Extension(
                proto_api::ExtensionTask {
                    spec: Some(proto_api::RawExtension {
                        raw: serde_json::to_vec(&serde_json::json!({ "command": command }))
                            .unwrap(),
                    }),
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

    fn make_valid_task_spec() -> proto_api::TaskSpec {
        proto_api::TaskSpec {
            slot: "test-slot".to_string(),
            workload: Some(make_workload("ls")),
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
    fn task_spec_routed_workload_valid() {
        let cs = convert_task_spec(make_valid_task_spec()).unwrap();
        assert_eq!(cs.slot(), "test-slot");
        assert_eq!(cs.timeout().as_millis(), 5_000);
        let TaskWorkload::Routed(workload) = cs.workload() else {
            panic!("expected a routed workload");
        };
        assert_eq!(workload.api_version(), "tasks.example.io/v1");
        assert_eq!(workload.kind(), "Echo");
        assert_eq!(workload.body()["command"], "ls");
        assert!(matches!(cs.restart(), RestartPolicy::OnFailure));
        assert!(matches!(cs.admission(), AdmissionPolicy::DropIfRunning));
        assert_eq!(cs.backoff().first_ms, 100);
        assert_eq!(cs.backoff().max_ms, 10_000);
    }

    #[test]
    fn chain_workload_round_trips_through_raw_extension() {
        let echo = |command: &str| {
            Workload::new(
                "tasks.example.io/v1",
                "Echo",
                serde_json::json!({ "command": command, "args": ["--verbose"] }),
            )
            .unwrap()
        };
        let expected = ChainSpec::new(
            "prepare",
            vec![
                ChainStep::new("prepare", echo("prepare"))
                    .unwrap()
                    .with_on_success("run")
                    .unwrap(),
                ChainStep::new("run", echo("run"))
                    .unwrap()
                    .with_on_failure("recover", FailureMode::Recover)
                    .unwrap(),
                ChainStep::new("recover", echo("recover")).unwrap(),
            ],
        )
        .unwrap();
        let workload = TaskWorkload::from_body(&expected).unwrap();

        let proto = workload_to_proto(&workload).unwrap();
        assert_eq!(proto.api_version, CHAIN_API_VERSION);
        assert_eq!(proto.kind, CHAIN_KIND);
        let Some(proto_api::task_workload::Spec::Extension(extension)) = proto.spec.as_ref() else {
            panic!("expected Chain to use ExtensionTask");
        };
        let raw = extension
            .spec
            .as_ref()
            .expect("expected Chain to use RawExtension");
        let raw_json: serde_json::Value = serde_json::from_slice(&raw.raw).unwrap();
        assert_eq!(raw_json["entry"], "prepare");
        assert_eq!(raw_json["steps"][0]["workload"]["kind"], "Echo");
        assert_eq!(raw_json["steps"][1]["onFailure"]["next"], "recover");
        assert_eq!(raw_json["steps"][1]["onFailure"]["mode"], "recover");

        let TaskWorkload::Routed(converted) = convert_task_workload(proto).unwrap() else {
            panic!("expected a routed workload");
        };
        let actual = converted.decode::<ChainSpec>().unwrap();
        assert_eq!(actual, expected);
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
                api_version: "tasks.example.io/v1".to_owned(),
                kind: "Echo".into(),
                spec: None,
            }),
            ..make_valid_task_spec()
        };
        let err = convert_task_spec(spec).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("extension.spec.raw")));
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
