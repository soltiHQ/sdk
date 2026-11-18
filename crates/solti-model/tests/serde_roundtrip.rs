use solti_model::{
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, LabelSelector, Labels,
    RestartPolicy, SelectorRequirement, Task, TaskPhase, TaskRun, TaskSpec, TaskWorkload, Workload,
};

fn embedded(revision: &str) -> TaskWorkload {
    TaskWorkload::Embedded(EmbeddedSpec::new(revision).unwrap())
}

fn fully_populated_spec(workload: TaskWorkload) -> TaskSpec {
    let mut selector_labels = Labels::new();
    selector_labels.insert("zone", "eu-west-1");

    let selector = LabelSelector {
        match_labels: selector_labels,
        match_expressions: vec![
            SelectorRequirement::exists("gpu"),
            SelectorRequirement::not_in("tainted", vec!["true".into()]),
        ],
    };

    TaskSpec::builder("build-pipeline", workload, 30_000u64)
        .restart(RestartPolicy::periodic(60_000))
        .backoff(BackoffPolicy {
            jitter: JitterPolicy::Equal,
            first_ms: 500,
            max_ms: 60_000,
            factor: 1.75,
        })
        .admission(AdmissionPolicy::Replace)
        .runner_selector(selector)
        .build()
        .expect("spec must build cleanly")
}

fn roundtrip(task: &Task) {
    let json1 = serde_json::to_string(task).expect("serialize");
    let back: Task = serde_json::from_str(&json1).expect("deserialize");
    let json2 = serde_json::to_string(&back).expect("reserialize");
    assert_eq!(json1, json2, "roundtrip drift");
}

fn routed(api_version: &str, kind: &str, body: serde_json::Value) -> TaskWorkload {
    TaskWorkload::Routed(Workload::new(api_version, kind, body).unwrap())
}

#[test]
fn routed_workload_variants_roundtrip() {
    let spec = fully_populated_spec(routed(
        "exec.solti.io/v1",
        "Subprocess",
        serde_json::json!({
            "mode": {"command": "/usr/bin/make", "args": ["-j4", "test"]},
            "env": [{"key": "LANG", "value": "C.UTF-8"}],
            "cwd": "/workspace",
            "failOnNonZero": true
        }),
    ));
    let mut task = Task::new("task-sub-cmd", spec).unwrap();
    task.transition_starting(1, 1, "1").unwrap();
    task.transition_finished(1, 1, TaskPhase::Failed, Some("retry".into()), None, "2")
        .unwrap();
    task.transition_starting(1, 2, "3").unwrap();
    task.transition_finished(1, 2, TaskPhase::Failed, Some("retry-2".into()), None, "4")
        .unwrap();
    task.transition_starting(1, 3, "5").unwrap();
    let command = task;

    let spec = fully_populated_spec(routed(
        "chain.solti.io/v1alpha1",
        "Chain",
        serde_json::json!({"steps": [{"name": "a"}, {"name": "b"}]}),
    ));
    let chain = Task::new("task-chain", spec).unwrap();

    let spec = fully_populated_spec(routed(
        "exec.solti.io/v1",
        "Container",
        serde_json::json!({
            "image": "registry.example.com/build:v1.2.3",
            "command": ["sh", "-c"],
            "args": ["echo hi"]
        }),
    ));
    let mut task = Task::new("task-ctr", spec).unwrap();
    task.transition_starting(1, 1, "1").unwrap();
    task.transition_finished(
        1,
        1,
        TaskPhase::Failed,
        Some("SIGKILL".into()),
        Some(137),
        "2",
    )
    .unwrap();
    let container = task;

    let embedded = Task::new("task-embedded", fully_populated_spec(embedded("test-v1"))).unwrap();

    for task in [&command, &chain, &container, &embedded] {
        roundtrip(task);
    }
}

#[test]
fn routed_body_preserves_unicode_exactly_in_json() {
    let workload = routed(
        "tasks.example.io/v1",
        "Report",
        serde_json::json!({"cwd": "/工作/δ", "module": "/модули/报告.wasm"}),
    );
    let json = serde_json::to_value(&workload).unwrap();
    assert_eq!(json["spec"]["cwd"], "/工作/δ");
    assert_eq!(json["spec"]["module"], "/модули/报告.wasm");

    let decoded: TaskWorkload = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, workload);
}

#[test]
fn roundtrip_embedded_bypasses_submit_validation() {
    let spec = TaskSpec::builder("internal-sync", embedded("test-v1"), 1_000u64)
        .build()
        .unwrap();
    let task = Task::new("task-emb", spec).unwrap();
    roundtrip(&task);
}

#[test]
fn task_run_roundtrip_preserves_all_fields() {
    let mut run = TaskRun::starting(3, 7, embedded("test-v1").type_meta()).unwrap();
    run.finish(
        TaskPhase::Exhausted,
        Some("retries exhausted".into()),
        Some(42),
    )
    .unwrap();
    let json1 = serde_json::to_string(&run).unwrap();
    let back: TaskRun = serde_json::from_str(&json1).unwrap();
    let json2 = serde_json::to_string(&back).unwrap();
    assert_eq!(json1, json2);
}

#[test]
fn backoff_invalid_on_wire_rejected_when_nested_in_spec() {
    let valid = fully_populated_spec(embedded("test-v1"));
    let mut json: serde_json::Value = serde_json::to_value(&valid).unwrap();
    json["backoff"]["firstMs"] = serde_json::json!(0);
    let err = serde_json::from_value::<TaskSpec>(json).unwrap_err();
    assert!(err.to_string().contains("first_ms"), "got: {err}");
}
