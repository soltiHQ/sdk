use solti_chain::{
    CHAIN_API_VERSION, CHAIN_KIND, ChainSpec, ChainStep, FailureMode, FailureTransition,
    is_chain_workload,
};
use solti_model::{LabelSelector, Labels, TaskWorkload, Workload, workload_schema_for};

fn subprocess() -> Workload {
    Workload::new(
        "exec.solti.io/v1",
        "Subprocess",
        serde_json::json!({ "mode": { "command": "echo" } }),
    )
    .unwrap()
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
fn chain_step_refuses_a_nested_chain() {
    let inner = ChainSpec::new(
        "inner",
        vec![ChainStep::new("inner", subprocess()).unwrap()],
    )
    .unwrap();
    let nested = Workload::from_body(&inner).unwrap();
    assert!(is_chain_workload(&nested));

    let error = ChainStep::new("nested", nested).unwrap_err();
    assert!(error.to_string().contains("nested"), "{error}");
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
fn chain_builds_from_a_routed_workload() {
    let spec = valid_chain();
    let workload = Workload::from_body(&spec).unwrap();

    assert!(is_chain_workload(&workload));
    assert_eq!(workload.api_version(), CHAIN_API_VERSION);
    assert_eq!(workload.kind(), CHAIN_KIND);
    assert_eq!(workload.decode::<ChainSpec>().unwrap(), spec);
    assert_eq!(
        TaskWorkload::from_body(&spec).unwrap(),
        TaskWorkload::Routed(workload)
    );

    assert!(!is_chain_workload(&subprocess()));
    let other = Workload::new("workflow.example.io/v1", "Chain", serde_json::json!({})).unwrap();
    assert!(!is_chain_workload(&other));
    assert!(other.decode::<ChainSpec>().is_err());
}

#[test]
fn decoding_a_chain_runs_the_graph_validation() {
    let cyclic = Workload::new(
        CHAIN_API_VERSION,
        CHAIN_KIND,
        serde_json::json!({
            "entry": "a",
            "steps": [
                { "name": "a", "workload": serde_json::to_value(subprocess()).unwrap(), "onSuccess": "b" },
                { "name": "b", "workload": serde_json::to_value(subprocess()).unwrap(), "onSuccess": "a" }
            ]
        }),
    )
    .unwrap();

    let error = cyclic.decode::<ChainSpec>().unwrap_err();
    assert!(error.detail().contains("acyclic"), "{error}");
}

#[test]
fn json_schema_is_strict_and_excludes_forbidden_workloads() {
    let schema = workload_schema_for::<ChainSpec>();
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
                "apiVersion": "solti.io/v1",
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
