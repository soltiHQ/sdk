use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, ContainerSpec, EmbeddedSpec, Flag, JitterPolicy, Labels,
    RestartPolicy, RunnerSelector, Runtime, SelectorRequirement, SubprocessMode, SubprocessSpec,
    Task, TaskEnv, TaskPhase, TaskRun, TaskSpec, TaskWorkload, WasmSpec,
};

fn embedded(revision: &str) -> TaskWorkload {
    TaskWorkload::Embedded(EmbeddedSpec::new(revision).unwrap())
}

fn fully_populated_spec(workload: TaskWorkload) -> TaskSpec {
    let mut selector_labels = Labels::new();
    selector_labels.insert("zone", "eu-west-1");

    let selector = RunnerSelector {
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

#[test]
fn roundtrip_subprocess_command() {
    let spec = fully_populated_spec(TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "/usr/bin/make".into(),
            args: vec!["-j4".into(), "test".into()],
        },
        {
            let mut e = TaskEnv::new();
            e.push("LANG", "C.UTF-8");
            e.push("BUILD_ID", "42");
            e
        },
        Some(std::path::PathBuf::from("/workspace")),
        Flag::enabled(),
    )));
    let mut task = Task::new("task-sub-cmd", spec).unwrap();
    task.transition_starting(1, 1, "1").unwrap();
    task.transition_finished(1, 1, TaskPhase::Failed, Some("retry".into()), None, "2")
        .unwrap();
    task.transition_starting(1, 2, "3").unwrap();
    task.transition_finished(1, 2, TaskPhase::Failed, Some("retry-2".into()), None, "4")
        .unwrap();
    task.transition_starting(1, 3, "5").unwrap();
    roundtrip(&task);
}

#[test]
fn roundtrip_subprocess_script_custom() {
    let spec = fully_populated_spec(TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Script {
            runtime: Runtime::Custom {
                command: "ruby".into(),
                flag: "-e".into(),
            },
            body: BASE64.encode(b"puts 'ok'"),
            args: vec![],
        },
        TaskEnv::new(),
        None,
        Flag::disabled(),
    )));
    let task = Task::new("task-script", spec).unwrap();
    roundtrip(&task);
}

#[test]
fn roundtrip_wasm() {
    let spec = fully_populated_spec(TaskWorkload::Wasm(WasmSpec::new(
        std::path::PathBuf::from("/modules/report.wasm"),
        vec!["--format=json".into()],
        TaskEnv::new(),
    )));
    let task = Task::new("task-wasm", spec).unwrap();
    roundtrip(&task);
}

#[test]
fn roundtrip_container() {
    let spec = fully_populated_spec(TaskWorkload::Container(ContainerSpec::new(
        "registry.example.com/build:v1.2.3".into(),
        Some(vec!["sh".into(), "-c".into()]),
        vec!["echo hi".into()],
        TaskEnv::new(),
    )));
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
    roundtrip(&task);
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
    let mut run = TaskRun::starting(3, 7, embedded("test-v1").type_meta());
    run.finish(
        TaskPhase::Exhausted,
        Some("retries exhausted".into()),
        Some(42),
    );
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
