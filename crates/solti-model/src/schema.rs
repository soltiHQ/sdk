//! JSON Schema helpers for custom wire encodings.

use schemars::{Schema, json_schema};

pub(crate) fn non_empty_string(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "pattern": "\\S"
    })
}

pub(crate) fn path_string(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1
    })
}

pub(crate) fn task_id(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": crate::TASK_ID_MAX_LEN,
        "pattern": "^[a-z0-9](?:[-a-z0-9]*[a-z0-9])?(?:\\.[a-z0-9](?:[-a-z0-9]*[a-z0-9])?)*$"
    })
}

pub(crate) fn slot(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": crate::SLOT_MAX_LEN,
        "pattern": "^[A-Za-z0-9._-]+$",
        "not": { "enum": [".", ".."] }
    })
}

pub(crate) fn agent_id(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": crate::AGENT_ID_MAX_LEN,
        "pattern": "^[A-Za-z0-9._-]+$",
        "not": { "enum": [".", ".."] }
    })
}

pub(crate) fn task_api_version(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "const": crate::TASK_API_VERSION
    })
}

pub(crate) fn task_kind(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "const": crate::TASK_KIND
    })
}

pub(crate) fn uid(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "pattern": "\\S"
    })
}

pub(crate) fn labels(generator: &mut schemars::SchemaGenerator) -> Schema {
    let key = qualified_name(generator);
    json_schema!({
        "type": "object",
        "propertyNames": key,
        "additionalProperties": {
            "type": "string",
            "maxLength": 63,
            "pattern": "^(?:[A-Za-z0-9](?:[-A-Za-z0-9_.]*[A-Za-z0-9])?)?$"
        }
    })
}

pub(crate) fn annotations(generator: &mut schemars::SchemaGenerator) -> Schema {
    let key = qualified_name(generator);
    json_schema!({
        "type": "object",
        "propertyNames": key,
        "additionalProperties": { "type": "string" }
    })
}

pub(crate) fn script_body(_generator: &mut schemars::SchemaGenerator) -> Schema {
    let max_encoded_len = crate::MAX_SCRIPT_BODY_BYTES.div_ceil(3) * 4;
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": max_encoded_len,
        "contentEncoding": "base64"
    })
}

pub(crate) fn condition_type(_generator: &mut schemars::SchemaGenerator) -> Schema {
    qualified_name_with_max(crate::resource::CONDITION_TYPE_MAX_BYTES)
}

pub(crate) fn condition_reason(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": crate::resource::CONDITION_REASON_MAX_BYTES,
        "pattern": "^[A-Za-z](?:[A-Za-z0-9_,:]*[A-Za-z0-9_])?$"
    })
}

pub(crate) fn qualified_name(_generator: &mut schemars::SchemaGenerator) -> Schema {
    qualified_name_with_max(
        crate::validation::DNS1123_SUBDOMAIN_MAX_LEN
            + 1
            + crate::validation::QUALIFIED_NAME_MAX_LEN,
    )
}

fn qualified_name_with_max(max_length: usize) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": max_length,
        "oneOf": [
            {
                "maxLength": crate::validation::QUALIFIED_NAME_MAX_LEN,
                "pattern": "^[A-Za-z0-9](?:[-A-Za-z0-9_.]{0,61}[A-Za-z0-9])?$"
            },
            {
                "pattern": "^(?=.{1,253}/)[a-z0-9](?:[-a-z0-9]*[a-z0-9])?(?:\\.[a-z0-9](?:[-a-z0-9]*[a-z0-9])?)*/[A-Za-z0-9](?:[-A-Za-z0-9_.]{0,61}[A-Za-z0-9])?$"
            }
        ]
    })
}

pub(crate) fn label_value(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "maxLength": crate::validation::QUALIFIED_NAME_MAX_LEN,
        "pattern": "^(?:[A-Za-z0-9](?:[-A-Za-z0-9_.]{0,61}[A-Za-z0-9])?)?$"
    })
}

pub(crate) fn runner_name(generator: &mut schemars::SchemaGenerator) -> Schema {
    let value = label_value(generator);
    json_schema!({
        "allOf": [
            value,
            { "minLength": 1 }
        ]
    })
}

pub(crate) fn json_object(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object",
        "additionalProperties": true
    })
}

pub(crate) fn crd_api_version(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 4,
        "maxLength": crate::validation::DNS1123_SUBDOMAIN_MAX_LEN
            + 1
            + crate::validation::DNS1035_LABEL_MAX_LEN,
        "pattern": "^(?=.{1,253}/)[a-z0-9](?:[-a-z0-9]*[a-z0-9])?(?:\\.[a-z0-9](?:[-a-z0-9]*[a-z0-9])?)+/[a-z](?:[-a-z0-9]{0,61}[a-z0-9])?$"
    })
}

pub(crate) fn extension_api_version(generator: &mut schemars::SchemaGenerator) -> Schema {
    let api_version = crd_api_version(generator);
    json_schema!({
        "allOf": [
            api_version,
            {
                "not": {
                    "pattern": "^solti\\.io/"
                }
            }
        ]
    })
}

pub(crate) fn crd_kind(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": crate::validation::DNS1035_LABEL_MAX_LEN,
        "pattern": "^[A-Za-z](?:[-A-Za-z0-9]*[A-Za-z0-9])?$"
    })
}

pub(crate) fn selector_requirement(generator: &mut schemars::SchemaGenerator) -> Schema {
    let key = qualified_name(generator);
    let value = label_value(generator);
    json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["key", "operator"],
        "properties": {
            "key": key,
            "operator": {
                "type": "string",
                "enum": ["In", "NotIn", "Exists", "DoesNotExist"]
            },
            "values": {
                "type": "array",
                "items": value
            }
        },
        "oneOf": [
            {
                "required": ["values"],
                "properties": {
                    "operator": { "enum": ["In", "NotIn"] },
                    "values": { "minItems": 1 }
                }
            },
            {
                "properties": {
                    "operator": { "enum": ["Exists", "DoesNotExist"] },
                    "values": { "maxItems": 0 }
                }
            }
        ]
    })
}

pub(crate) fn runner_workload_types(generator: &mut schemars::SchemaGenerator) -> Schema {
    let workload = generator.subschema_for::<crate::WorkloadTypeMeta>();
    json_schema!({
        "type": "array",
        "minItems": 1,
        "uniqueItems": true,
        "items": {
            "allOf": [
                workload,
                {
                    "not": {
                        "type": "object",
                        "required": ["apiVersion", "kind"],
                        "properties": {
                            "apiVersion": { "const": crate::WORKLOAD_API_VERSION },
                            "kind": { "const": "Embedded" }
                        }
                    }
                }
            ]
        }
    })
}

pub(crate) fn task_run(generator: &mut schemars::SchemaGenerator) -> Schema {
    let workload = generator.subschema_for::<crate::WorkloadTypeMeta>();
    let started_at = rfc3339_time(generator);
    let finished_at = optional_rfc3339_time(generator);
    let diagnostic = task_diagnostic(generator);
    json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workload", "generation", "attempt", "phase", "startedAt"],
        "properties": {
            "workload": workload,
            "generation": {
                "type": "integer",
                "format": "uint64",
                "minimum": 1
            },
            "attempt": {
                "type": "integer",
                "format": "uint32",
                "minimum": 1
            },
            "phase": {
                "type": "string",
                "enum": [
                    "running",
                    "succeeded",
                    "failed",
                    "timeout",
                    "canceled",
                    "exhausted"
                ]
            },
            "startedAt": started_at,
            "finishedAt": finished_at,
            "error": diagnostic,
            "exitCode": {
                "type": ["integer", "null"],
                "format": "int32"
            }
        },
        "oneOf": [
            {
                "properties": {
                    "phase": { "const": "running" },
                    "finishedAt": { "type": "null" },
                    "error": { "type": "null" },
                    "exitCode": { "type": "null" }
                }
            },
            {
                "required": ["finishedAt"],
                "properties": {
                    "phase": {
                        "enum": [
                            "succeeded",
                            "failed",
                            "timeout",
                            "canceled",
                            "exhausted"
                        ]
                    },
                    "finishedAt": {
                        "type": "string",
                        "format": "date-time"
                    }
                }
            }
        ]
    })
}

pub(crate) fn task_status(generator: &mut schemars::SchemaGenerator) -> Schema {
    let conditions = generator.subschema_for::<Vec<crate::TaskCondition>>();
    let diagnostic = task_diagnostic(generator);
    json_schema!({
        "type": "object",
        "additionalProperties": false,
        "required": ["observedGeneration", "phase", "attempt", "conditions"],
        "properties": {
            "observedGeneration": {
                "type": "integer",
                "format": "uint64",
                "minimum": 0
            },
            "phase": {
                "type": "string",
                "enum": [
                    "pending",
                    "running",
                    "succeeded",
                    "failed",
                    "timeout",
                    "canceled",
                    "exhausted"
                ]
            },
            "attempt": {
                "type": "integer",
                "format": "uint32",
                "minimum": 0
            },
            "exitCode": {
                "type": ["integer", "null"],
                "format": "int32"
            },
            "error": diagnostic,
            "conditions": {
                "allOf": [
                    conditions,
                    { "minItems": 1 },
                    {
                        "contains": {
                            "type": "object",
                            "required": ["type"],
                            "properties": {
                                "type": { "const": "Reconciled" }
                            }
                        },
                        "minContains": 1,
                        "maxContains": 1
                    },
                    {
                        "contains": {
                            "type": "object",
                            "required": ["type", "observedGeneration"],
                            "properties": {
                                "type": { "const": "Reconciled" },
                                "observedGeneration": { "minimum": 1 }
                            }
                        },
                        "minContains": 1
                    }
                ]
            }
        },
        "oneOf": [
            {
                "properties": {
                    "phase": { "const": "pending" },
                    "attempt": { "const": 0 },
                    "exitCode": { "type": "null" },
                    "error": { "type": "null" }
                }
            },
            {
                "properties": {
                    "phase": { "const": "running" },
                    "attempt": { "minimum": 1 },
                    "exitCode": { "type": "null" },
                    "error": { "type": "null" },
                    "conditions": {
                        "contains": {
                            "type": "object",
                            "required": ["type", "status"],
                            "properties": {
                                "type": { "const": "Reconciled" },
                                "status": { "const": "True" }
                            }
                        },
                        "minContains": 1
                    }
                }
            },
            {
                "properties": {
                    "phase": {
                        "enum": [
                            "succeeded",
                            "failed",
                            "timeout",
                            "canceled",
                            "exhausted"
                        ]
                    },
                    "conditions": {
                        "contains": {
                            "type": "object",
                            "required": ["type", "status"],
                            "properties": {
                                "type": { "const": "Reconciled" },
                                "status": { "const": "True" }
                            }
                        },
                        "minContains": 1
                    }
                }
            }
        ]
    })
}

fn task_diagnostic(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": ["string", "null"],
        "maxLength": crate::MAX_TASK_DIAGNOSTIC_BYTES,
        "description": "Runtime values are normalized to the longest UTF-8-safe prefix of at most 32768 bytes. JSON Schema maxLength is the corresponding Unicode code-point ceiling, not a byte measurement."
    })
}

pub(crate) fn rfc3339_time(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "format": "date-time"
    })
}

pub(crate) fn optional_rfc3339_time(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": ["string", "null"],
        "format": "date-time"
    })
}

pub(crate) fn unix_millis(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "integer",
        "format": "uint64",
        "minimum": 0
    })
}

pub(crate) fn base64_bytes(_generator: &mut schemars::SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "contentEncoding": "base64"
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use jsonschema::Validator;
    use schemars::JsonSchema;
    use serde::Serialize;
    use serde_json::{Value, json};

    use crate::{
        AgentCapabilities, Annotations, ContainerSpec, EmbeddedSpec, ExtensionWorkload, Flag,
        Labels, RunnerCapability, SelectorRequirement, SubprocessMode, SubprocessSpec,
        TaskConditionType, TaskEnv, TaskManifest, TaskRun, TaskSpec, TaskStatus, TaskWorkload,
        WORKLOAD_API_VERSION, WasmSpec, WorkloadTypeMeta,
    };

    fn validator<T: JsonSchema>() -> Validator {
        let schema = serde_json::to_value(schemars::schema_for!(T)).unwrap();
        jsonschema::meta::validate(&schema).unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    fn assert_valid<T>(value: &T)
    where
        T: JsonSchema + Serialize,
    {
        let document = serde_json::to_value(value).unwrap();
        let validator = validator::<T>();
        assert!(
            validator.is_valid(&document),
            "{} does not match its schema: {document}",
            std::any::type_name::<T>(),
        );
    }

    fn manifest(workload: TaskWorkload) -> TaskManifest {
        let spec = TaskSpec::builder("schema-test", workload, 1_000u64)
            .build()
            .unwrap();
        TaskManifest::new("schema-test", spec).unwrap()
    }

    #[test]
    fn task_manifest_schema_accepts_every_model_workload() {
        let workloads = [
            TaskWorkload::Subprocess(SubprocessSpec::new(
                SubprocessMode::Command {
                    command: "echo".into(),
                    args: vec!["hello".into()],
                },
                TaskEnv::default(),
                None,
                Flag::enabled(),
            )),
            TaskWorkload::Wasm(WasmSpec::new(
                PathBuf::from("job.wasm"),
                Vec::new(),
                TaskEnv::default(),
            )),
            TaskWorkload::Container(ContainerSpec::new(
                "docker.io/library/busybox:1".into(),
                None,
                Vec::new(),
                TaskEnv::default(),
            )),
            TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
            TaskWorkload::Extension(
                ExtensionWorkload::new(
                    "workloads.example.io/v1",
                    "Resize",
                    json!({ "width": 320 }),
                )
                .unwrap(),
            ),
        ];

        for workload in workloads {
            assert_valid(&manifest(workload));
        }
    }

    #[test]
    fn task_manifest_schema_accepts_unicode_wire_paths() {
        let workloads = [
            TaskWorkload::Subprocess(SubprocessSpec::new(
                SubprocessMode::Command {
                    command: "echo".into(),
                    args: Vec::new(),
                },
                TaskEnv::default(),
                Some(PathBuf::from("/工作/δ")),
                Flag::enabled(),
            )),
            TaskWorkload::Wasm(WasmSpec::new(
                PathBuf::from("/модули/报告.wasm"),
                Vec::new(),
                TaskEnv::default(),
            )),
        ];

        for workload in workloads {
            assert_valid(&manifest(workload));
        }
    }

    #[test]
    fn selector_schema_matches_operator_and_value_rules() {
        let schema = validator::<SelectorRequirement>();
        for valid in [
            serde_json::to_value(SelectorRequirement::exists("gpu")).unwrap(),
            serde_json::to_value(SelectorRequirement::r#in("gpu", vec!["a100".into()])).unwrap(),
        ] {
            assert!(schema.is_valid(&valid), "expected valid: {valid}");
        }

        for invalid in [
            json!({ "key": "bad key", "operator": "Exists" }),
            json!({ "key": "gpu", "operator": "In" }),
            json!({ "key": "gpu", "operator": "In", "values": [] }),
            json!({ "key": "gpu", "operator": "Exists", "values": ["a100"] }),
            json!({ "key": "gpu", "operator": "In", "values": ["-invalid"] }),
        ] {
            assert!(!schema.is_valid(&invalid), "expected invalid: {invalid}");
        }
    }

    #[test]
    fn task_run_schema_rejects_impossible_lifecycle_shapes() {
        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        let mut active = TaskRun::starting(1, 1, workload).unwrap();
        assert_valid(&active);

        active
            .finish(crate::TaskPhase::Succeeded, None, Some(0))
            .unwrap();
        assert_valid(&active);

        let schema = validator::<TaskRun>();
        let base = serde_json::to_value(active).unwrap();
        for invalid in [
            with_fields(&base, &[("phase", json!("pending"))], &[]),
            with_fields(&base, &[("phase", json!("running"))], &[]),
            with_fields(&base, &[], &["finishedAt"]),
        ] {
            assert!(!schema.is_valid(&invalid), "expected invalid: {invalid}");
        }
    }

    #[test]
    fn task_status_schema_enforces_reconciled_lifecycle_shape() {
        let schema = validator::<TaskStatus>();
        let pending = serde_json::to_value(TaskStatus::pending(1).unwrap()).unwrap();
        assert!(schema.is_valid(&pending));

        let reconciled = pending["conditions"][0].clone();
        let mut extension = reconciled.clone();
        extension["type"] = json!("example.io/Available");
        extension["status"] = json!("False");

        let mut extended = pending.clone();
        extended["conditions"] = json!([reconciled.clone(), extension.clone()]);
        assert!(schema.is_valid(&extended));

        let mut duplicate = reconciled.clone();
        duplicate["reason"] = json!("Duplicate");
        duplicate["message"] = json!("duplicate condition");

        let mut zero_generation = reconciled.clone();
        zero_generation["observedGeneration"] = json!(0);

        let mut running_unknown = pending.clone();
        running_unknown["phase"] = json!("running");
        running_unknown["attempt"] = json!(1);

        let mut terminal_false = pending.clone();
        terminal_false["phase"] = json!("failed");
        terminal_false["conditions"][0]["status"] = json!("False");

        for invalid in [
            with_fields(&pending, &[("conditions", json!([extension]))], &[]),
            with_fields(
                &pending,
                &[("conditions", json!([reconciled.clone(), duplicate]))],
                &[],
            ),
            with_fields(&pending, &[("conditions", json!([zero_generation]))], &[]),
            running_unknown,
            terminal_false,
        ] {
            assert!(!schema.is_valid(&invalid), "expected invalid: {invalid}");
        }

        let mut task = crate::Task::from_manifest(manifest(TaskWorkload::Embedded(
            EmbeddedSpec::new("test-v1").unwrap(),
        )))
        .unwrap();
        task.transition_starting(1, 1, "1").unwrap();
        assert!(schema.is_valid(&serde_json::to_value(task.status()).unwrap()));
        task.transition_finished(1, 1, crate::TaskPhase::Succeeded, None, Some(0), "2")
            .unwrap();
        assert!(schema.is_valid(&serde_json::to_value(task.status()).unwrap()));
    }

    #[test]
    fn task_diagnostic_schemas_expose_the_code_point_ceiling() {
        let exact = "a".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES);
        let oversized = "a".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES + 1);
        let multibyte_over_byte_budget = "🙂".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES / 4 + 1);

        let workload = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        let mut run = TaskRun::starting(1, 1, workload).unwrap();
        run.finish(crate::TaskPhase::Failed, Some("failure".into()), None)
            .unwrap();
        let run = serde_json::to_value(run).unwrap();
        let run_schema = validator::<TaskRun>();
        assert!(run_schema.is_valid(&with_fields(&run, &[("error", json!(exact))], &[],)));
        assert!(!run_schema.is_valid(&with_fields(&run, &[("error", json!(oversized))], &[],)));
        assert!(run_schema.is_valid(&with_fields(
            &run,
            &[("error", json!(multibyte_over_byte_budget.clone()))],
            &[],
        )));

        let mut task = crate::Task::from_manifest(manifest(TaskWorkload::Embedded(
            EmbeddedSpec::new("test-v1").unwrap(),
        )))
        .unwrap();
        task.reconcile_finished(
            1,
            crate::TaskPhase::Failed,
            Some("failure".into()),
            None,
            "1",
        )
        .unwrap();
        let status = serde_json::to_value(task.status()).unwrap();
        let status_schema = validator::<TaskStatus>();
        assert!(status_schema.is_valid(&with_fields(
            &status,
            &[("error", json!("a".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES)),)],
            &[],
        )));
        assert!(!status_schema.is_valid(&with_fields(
            &status,
            &[(
                "error",
                json!("a".repeat(crate::MAX_TASK_DIAGNOSTIC_BYTES + 1)),
            )],
            &[],
        )));
        assert!(status_schema.is_valid(&with_fields(
            &status,
            &[("error", json!(multibyte_over_byte_budget))],
            &[],
        )));
    }

    #[test]
    fn capability_schema_rejects_empty_and_embedded_runner_contracts() {
        let capability = RunnerCapability::new(
            "subprocess",
            Labels::new(),
            vec![WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap()],
        )
        .unwrap();
        assert_valid(&capability);
        assert_valid(&AgentCapabilities::new(vec![capability]).unwrap());

        let schema = validator::<RunnerCapability>();
        assert!(!schema.is_valid(&json!({
            "name": "runner",
            "labels": {},
            "workloadTypes": []
        })));
        assert!(!schema.is_valid(&json!({
            "name": "runner",
            "labels": {},
            "workloadTypes": [{
                "apiVersion": WORKLOAD_API_VERSION,
                "kind": "Embedded"
            }]
        })));
    }

    #[test]
    fn qualified_name_schemas_match_component_limits() {
        let name_64 = "a".repeat(64);
        let prefix_253 = format!("{}.a", "a".repeat(251));
        let prefix_254 = format!("{}.a", "a".repeat(252));

        let mut annotations = Annotations::new();
        annotations.insert(&name_64, "value");
        assert!(annotations.validate().is_err());
        assert!(!validator::<Annotations>().is_valid(&serde_json::to_value(&annotations).unwrap()));

        let mut labels = Labels::new();
        labels.insert(&name_64, "value");
        assert!(labels.validate().is_err());
        assert!(!validator::<Labels>().is_valid(&serde_json::to_value(&labels).unwrap()));

        let selector_key = format!("{prefix_254}/name");
        assert!(
            SelectorRequirement::exists(&selector_key)
                .validate()
                .is_err()
        );
        assert!(!validator::<SelectorRequirement>().is_valid(&json!({
            "key": selector_key,
            "operator": "Exists"
        })));

        let valid_condition = format!("{prefix_253}/{}", "a".repeat(62));
        let oversized_condition = format!("{prefix_253}/{}", "a".repeat(63));
        assert!(TaskConditionType::new(&valid_condition).is_ok());
        assert!(TaskConditionType::new(&oversized_condition).is_err());
        let condition_schema = validator::<TaskConditionType>();
        assert!(condition_schema.is_valid(&json!(valid_condition)));
        assert!(!condition_schema.is_valid(&json!(oversized_condition)));
    }

    #[test]
    fn crd_api_version_schemas_match_component_limits() {
        let group_253 = format!("{}.a", "a".repeat(251));
        let group_254 = format!("{}.a", "a".repeat(252));
        let version_63 = format!("v{}", "1".repeat(62));
        let version_64 = format!("v{}", "1".repeat(63));

        let valid = format!("{group_253}/{version_63}");
        let oversized_group = format!("{group_254}/v1");
        let oversized_version = format!("example.io/{version_64}");
        assert!(WorkloadTypeMeta::new(&valid, "Example").is_ok());
        assert!(WorkloadTypeMeta::new(&oversized_group, "Example").is_err());
        assert!(WorkloadTypeMeta::new(&oversized_version, "Example").is_err());

        let workload_schema = validator::<WorkloadTypeMeta>();
        assert!(workload_schema.is_valid(&json!({
            "apiVersion": valid,
            "kind": "Example"
        })));
        for invalid in [&oversized_group, &oversized_version] {
            assert!(!workload_schema.is_valid(&json!({
                "apiVersion": invalid,
                "kind": "Example"
            })));
        }

        let extension_schema = validator::<ExtensionWorkload>();
        assert!(!extension_schema.is_valid(&json!({
            "apiVersion": oversized_group,
            "kind": "Example",
            "spec": {}
        })));
    }

    fn with_fields(base: &Value, replacements: &[(&str, Value)], removals: &[&str]) -> Value {
        let mut value = base.clone();
        let object = value.as_object_mut().unwrap();
        for (key, replacement) in replacements {
            object.insert((*key).to_owned(), replacement.clone());
        }
        for key in removals {
            object.remove(*key);
        }
        value
    }
}
