use solti_chain::{
    CHAIN_API_VERSION, CHAIN_KIND, ChainSpec, ChainStep, FailureMode, FailureTransition,
    is_chain_workload,
};
use solti_model::{
    EmbeddedSpec, ExtensionWorkload, Flag, LabelSelector, Labels, SubprocessMode, SubprocessSpec,
    TaskEnv, TaskWorkload, WORKLOAD_API_VERSION,
};

fn subprocess() -> TaskWorkload {
    TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec![],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ))
}

fn valid_chain() -> ChainSpec {
    ChainSpec::new(
        "task1",
        vec![
            ChainStep::new("task1", subprocess())
                .unwrap()
                .with_on_success("task2")
                .unwrap(),
            ChainStep::new("task2", subprocess())
                .unwrap()
                .with_on_success("task3")
                .unwrap(),
            ChainStep::new("task3", subprocess())
                .unwrap()
                .with_on_failure("task4", FailureMode::Recover)
                .unwrap(),
            ChainStep::new("task4", subprocess()).unwrap(),
        ],
    )
    .unwrap()
}

#[test]
fn accepts_an_outcome_directed_acyclic_chain() {
    let spec = valid_chain();

    assert_eq!(spec.entry().as_str(), "task1");
    assert_eq!(spec.steps().len(), 4);
    assert_eq!(
        spec.step("task3").unwrap().on_failure().unwrap().mode(),
        FailureMode::Recover
    );
    assert!(spec.step("missing").is_none());
    spec.validate().unwrap();
}

#[test]
fn names_use_the_existing_task_id_grammar() {
    let dotted = ChainStep::new("build.frontend", subprocess()).unwrap();
    ChainSpec::new("build.frontend", vec![dotted]).unwrap();

    for invalid in ["", "UPPER", "under_score", "-leading", "trailing-"] {
        assert!(
            ChainStep::new(invalid, subprocess()).is_err(),
            "must reject {invalid:?}"
        );
    }
}

#[test]
fn rejects_empty_duplicate_missing_unreachable_and_cyclic_graphs() {
    let empty = ChainSpec::new("entry", vec![]).unwrap_err();
    assert!(empty.to_string().contains("at least one"));

    let duplicate = ChainSpec::new(
        "same",
        vec![
            ChainStep::new("same", subprocess()).unwrap(),
            ChainStep::new("same", subprocess()).unwrap(),
        ],
    )
    .unwrap_err();
    assert!(duplicate.to_string().contains("duplicate"));

    let missing_entry = ChainSpec::new(
        "missing",
        vec![ChainStep::new("declared", subprocess()).unwrap()],
    )
    .unwrap_err();
    assert!(missing_entry.to_string().contains("entry 'missing'"));

    let missing_target = ChainSpec::new(
        "entry",
        vec![
            ChainStep::new("entry", subprocess())
                .unwrap()
                .with_on_success("missing")
                .unwrap(),
        ],
    )
    .unwrap_err();
    assert!(missing_target.to_string().contains("target 'missing'"));

    let unreachable = ChainSpec::new(
        "entry",
        vec![
            ChainStep::new("entry", subprocess()).unwrap(),
            ChainStep::new("orphan", subprocess()).unwrap(),
        ],
    )
    .unwrap_err();
    assert!(unreachable.to_string().contains("unreachable"));

    let cycle = ChainSpec::new(
        "a",
        vec![
            ChainStep::new("a", subprocess())
                .unwrap()
                .with_on_success("b")
                .unwrap(),
            ChainStep::new("b", subprocess())
                .unwrap()
                .with_on_success("a")
                .unwrap(),
        ],
    )
    .unwrap_err();
    assert!(cycle.to_string().contains("acyclic"));
}

#[test]
fn rejects_embedded_and_nested_chain_workloads() {
    let embedded = TaskWorkload::Embedded(EmbeddedSpec::new("v1").unwrap());
    let error = ChainStep::new("embedded", embedded).unwrap_err();
    assert!(error.to_string().contains("Embedded"));

    let nested: TaskWorkload = ChainSpec::new(
        "inner",
        vec![ChainStep::new("inner", subprocess()).unwrap()],
    )
    .unwrap()
    .try_into()
    .unwrap();
    let error = ChainStep::new("nested", nested).unwrap_err();
    assert!(error.to_string().contains("nested"));
}

#[test]
fn validates_step_runner_selectors() {
    let mut labels = Labels::new();
    labels.insert("bad key", "value");
    let invalid = LabelSelector::from_labels(labels);

    let error = ChainStep::new("step", subprocess())
        .unwrap()
        .with_runner_selector(invalid)
        .unwrap_err();
    assert!(error.to_string().contains("runnerSelector"));
}

#[test]
fn failure_mode_defaults_to_preserve_and_serializes_canonically() {
    let json = serde_json::json!({
        "entry": "work",
        "steps": [
            {
                "name": "work",
                "workload": serde_json::to_value(subprocess()).unwrap(),
                "onFailure": { "next": "handler" }
            },
            {
                "name": "handler",
                "workload": serde_json::to_value(subprocess()).unwrap()
            }
        ]
    });

    let spec: ChainSpec = serde_json::from_value(json).unwrap();
    let transition = spec.step("work").unwrap().on_failure().unwrap();
    assert_eq!(transition.mode(), FailureMode::Preserve);

    let encoded = serde_json::to_value(&spec).unwrap();
    assert_eq!(encoded["steps"][0]["onFailure"]["mode"], "preserve");

    let direct = FailureTransition::new("handler", FailureMode::default()).unwrap();
    assert_eq!(direct.mode(), FailureMode::Preserve);
}

#[test]
fn serde_is_strict_and_runs_runtime_validation() {
    let workload = serde_json::to_value(subprocess()).unwrap();
    let cases = [
        serde_json::json!({
            "entry": "only",
            "steps": [{ "name": "only", "workload": workload.clone() }],
            "unexpected": true
        }),
        serde_json::json!({
            "entry": "only",
            "steps": [{
                "name": "only",
                "workload": workload.clone(),
                "unexpected": true
            }]
        }),
        serde_json::json!({
            "entry": "only",
            "steps": [{
                "name": "only",
                "workload": workload.clone(),
                "onFailure": { "next": "handler", "mode": "preserve", "unexpected": true }
            }, {
                "name": "handler",
                "workload": workload.clone()
            }]
        }),
        serde_json::json!({
            "entry": "a",
            "steps": [
                { "name": "a", "workload": workload.clone(), "onSuccess": "b" },
                { "name": "b", "workload": workload.clone(), "onSuccess": "a" }
            ]
        }),
    ];

    for value in cases {
        assert!(
            serde_json::from_value::<ChainSpec>(value.clone()).is_err(),
            "must reject {value}"
        );
    }
}

#[test]
fn extension_workload_conversion_enforces_the_chain_gvk() {
    let spec = valid_chain();
    let workload: TaskWorkload = spec.clone().try_into().unwrap();

    assert!(is_chain_workload(&workload));
    assert_eq!(workload.api_version(), CHAIN_API_VERSION);
    assert_eq!(workload.kind(), CHAIN_KIND);
    assert_eq!(ChainSpec::from_workload(&workload).unwrap(), spec);
    assert_eq!(ChainSpec::try_from(workload.clone()).unwrap(), spec);

    let TaskWorkload::Extension(extension) = workload else {
        panic!("chain must use an extension workload");
    };
    assert_eq!(ChainSpec::try_from(&extension).unwrap(), spec);

    let other = TaskWorkload::Extension(
        ExtensionWorkload::new("workflow.example.io/v1", "Chain", serde_json::json!({})).unwrap(),
    );
    let error = ChainSpec::from_workload(&other).unwrap_err();
    assert!(error.to_string().contains(CHAIN_API_VERSION));

    let builtin = subprocess();
    let error = ChainSpec::from_workload(&builtin).unwrap_err();
    assert!(error.to_string().contains(WORKLOAD_API_VERSION));
}

#[cfg(feature = "schema")]
#[test]
fn json_schema_is_strict_and_excludes_forbidden_workloads() {
    let schema = serde_json::to_value(schemars::schema_for!(ChainSpec)).unwrap();
    jsonschema::meta::validate(&schema).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();

    let valid = serde_json::to_value(valid_chain()).unwrap();
    assert!(validator.is_valid(&valid));

    let mut unknown = valid.clone();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("unexpected".into(), serde_json::json!(true));
    assert!(!validator.is_valid(&unknown));

    assert!(!validator.is_valid(&serde_json::json!({
        "entry": "entry",
        "steps": []
    })));

    assert!(!validator.is_valid(&serde_json::json!({
        "entry": "entry",
        "steps": [{
            "name": "entry",
            "workload": {
                "apiVersion": WORKLOAD_API_VERSION,
                "kind": "Embedded",
                "spec": { "revision": "v1" }
            }
        }]
    })));

    assert!(!validator.is_valid(&serde_json::json!({
        "entry": "entry",
        "steps": [{
            "name": "entry",
            "workload": {
                "apiVersion": CHAIN_API_VERSION,
                "kind": CHAIN_KIND,
                "spec": {
                    "entry": "inner",
                    "steps": [{ "name": "inner", "workload": serde_json::to_value(subprocess()).unwrap() }]
                }
            }
        }]
    })));
}
