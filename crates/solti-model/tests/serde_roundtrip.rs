//! End-to-end serde roundtrips for `Task` with every non-embedded `TaskKind`
//! variant plus every optional `TaskSpec` field populated (labels, selector,
//! non-default policies). Guards against drift between `Serialize` (direct
//! field-by-field) and `Deserialize` (via `TaskSpecRaw::try_from`) when fields
//! or renames move under our feet.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use solti_model::{
    AdmissionPolicy, BackoffPolicy, ContainerSpec, Flag, JitterPolicy, Labels, RestartPolicy,
    RunnerSelector, Runtime, SelectorRequirement, SubprocessMode, SubprocessSpec, Task, TaskEnv,
    TaskKind, TaskPhase, TaskRun, TaskSpec, WasmSpec,
};

fn fully_populated_spec(kind: TaskKind) -> TaskSpec {
    let mut labels = Labels::new();
    labels.insert("tier", "prod").insert("owner", "platform");

    let mut selector_labels = Labels::new();
    selector_labels.insert("zone", "eu-west-1");

    let selector = RunnerSelector {
        match_labels: selector_labels,
        match_expressions: vec![
            SelectorRequirement::exists("gpu"),
            SelectorRequirement::not_in("tainted", vec!["true".into()]),
        ],
    };

    TaskSpec::builder("build-pipeline", kind, 30_000u64)
        .restart(RestartPolicy::periodic(60_000))
        .backoff(BackoffPolicy {
            jitter: JitterPolicy::Equal,
            first_ms: 500,
            max_ms: 60_000,
            factor: 1.75,
        })
        .admission(AdmissionPolicy::Replace)
        .runner_selector(selector)
        .labels(labels)
        .build()
        .expect("spec must build cleanly")
}

/// Compare by re-serializing both sides. Direct `Task` equality would fail
/// because [`std::time::SystemTime`] keeps nanosecond precision, while our
/// wire format is millisecond-precision — sub-ms digits get dropped on the
/// first trip. Once both sides go through the codec, they agree.
fn roundtrip(task: &Task) {
    let json1 = serde_json::to_string(task).expect("serialize");
    let back: Task = serde_json::from_str(&json1).expect("deserialize");
    let json2 = serde_json::to_string(&back).expect("reserialize");
    assert_eq!(json1, json2, "roundtrip drift");
}

#[test]
fn roundtrip_subprocess_command() {
    let spec = fully_populated_spec(TaskKind::Subprocess(SubprocessSpec {
        mode: SubprocessMode::Command {
            command: "/usr/bin/make".into(),
            args: vec!["-j4".into(), "test".into()],
        },
        env: {
            let mut e = TaskEnv::new();
            e.push("LANG", "C.UTF-8");
            e.push("BUILD_ID", "42");
            e
        },
        cwd: Some(std::path::PathBuf::from("/workspace")),
        fail_on_non_zero: Flag::enabled(),
    }));
    let mut task = Task::new("task-sub-cmd".into(), spec);
    task.status.phase = TaskPhase::Running;
    task.status.attempt = 3;
    roundtrip(&task);
}

#[test]
fn roundtrip_subprocess_script_custom() {
    let spec = fully_populated_spec(TaskKind::Subprocess(SubprocessSpec {
        mode: SubprocessMode::Script {
            runtime: Runtime::Custom {
                command: "ruby".into(),
                flag: "-e".into(),
            },
            body: BASE64.encode(b"puts 'ok'"),
            args: vec![],
        },
        env: TaskEnv::new(),
        cwd: None,
        fail_on_non_zero: Flag::disabled(),
    }));
    let task = Task::new("task-script".into(), spec);
    roundtrip(&task);
}

#[test]
fn roundtrip_wasm() {
    let spec = fully_populated_spec(TaskKind::Wasm(WasmSpec {
        module: std::path::PathBuf::from("/modules/report.wasm"),
        args: vec!["--format=json".into()],
        env: TaskEnv::new(),
    }));
    let task = Task::new("task-wasm".into(), spec);
    roundtrip(&task);
}

#[test]
fn roundtrip_container() {
    let spec = fully_populated_spec(TaskKind::Container(ContainerSpec {
        image: "registry.example.com/build:v1.2.3".into(),
        command: Some(vec!["sh".into(), "-c".into()]),
        args: vec!["echo hi".into()],
        env: TaskEnv::new(),
    }));
    let mut task = Task::new("task-ctr".into(), spec);
    task.status.phase = TaskPhase::Failed;
    task.status.exit_code = Some(137);
    task.status.error = Some("SIGKILL".into());
    roundtrip(&task);
}

#[test]
fn roundtrip_embedded_bypasses_submit_validation() {
    // TaskKind::Embedded is rejected by submit-boundary validate() but the
    // resource is still serializable — important for internal tasks stored in
    // state/exported for debugging.
    let spec = TaskSpec::builder("internal-sync", TaskKind::Embedded, 1_000u64)
        .build()
        .unwrap();
    let task = Task::new("task-emb".into(), spec);
    roundtrip(&task);
}

#[test]
fn task_run_roundtrip_preserves_all_fields() {
    let mut run = TaskRun::starting(7);
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
    // BackoffPolicy now validates in its own Deserialize path; a spec carrying a
    // zero first_ms must be rejected at parse time, not silently accepted.
    let valid = fully_populated_spec(TaskKind::Embedded);
    let mut json: serde_json::Value = serde_json::to_value(&valid).unwrap();
    json["backoff"]["firstMs"] = serde_json::json!(0);
    let err = serde_json::from_value::<TaskSpec>(json).unwrap_err();
    assert!(err.to_string().contains("first_ms"), "got: {err}");
}
