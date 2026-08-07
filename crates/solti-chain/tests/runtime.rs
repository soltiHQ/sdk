use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use serde_json::{Value, json};
use solti_chain::{ChainRunner, ChainSpec, ChainStep, FailureMode, register_chain_runner};
use solti_model::{
    ExtensionWorkload, LabelSelector, Labels, OutputEvent, StreamKind, Task, TaskId, TaskSpec,
    TaskWorkload, WorkloadTypeMeta,
};
use solti_runner::{
    BuildContext, OutputPublisher, OutputPublisherHandle, OutputSink, RunId, Runner, RunnerError,
    RunnerRouter,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

const TEST_API_VERSION: &str = "runtime.tests.example/v1";
const TEST_KIND: &str = "Leaf";
const MISSING_KIND: &str = "Missing";

#[derive(Clone)]
struct LeafRunner {
    name: &'static str,
    executions: Arc<Mutex<Vec<String>>>,
    builds: Arc<Mutex<Vec<String>>>,
}

impl LeafRunner {
    fn new(
        name: &'static str,
        executions: Arc<Mutex<Vec<String>>>,
        builds: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            name,
            executions,
            builds,
        }
    }
}

impl Runner for LeafRunner {
    fn name(&self) -> &str {
        self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![
            WorkloadTypeMeta::new(TEST_API_VERSION, TEST_KIND)
                .expect("test workload GVK must be valid"),
        ]
    }

    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let (id, behavior) = leaf_fields(task)?;
        self.builds
            .lock()
            .expect("build log mutex must not be poisoned")
            .push(format!("{}:{id}", self.name));

        let executions = Arc::clone(&self.executions);
        let output = Arc::clone(ctx.output_publisher());
        let resource_name = task.name().clone();
        let generation = task.metadata().generation();
        let runner_name = self.name.to_owned();
        let attempts = Arc::new(AtomicU32::new(0));

        Ok(TaskFn::arc(
            run_id.name().to_owned(),
            move |_ctx: TaskContext| {
                let executions = Arc::clone(&executions);
                let output = Arc::clone(&output);
                let resource_name = resource_name.clone();
                let id = id.clone();
                let behavior = behavior.clone();
                let runner_name = runner_name.clone();
                let attempt = attempts.fetch_add(1, Ordering::Relaxed).wrapping_add(1);

                async move {
                    executions
                        .lock()
                        .expect("execution log mutex must not be poisoned")
                        .push(format!("{runner_name}:{id}"));

                    if let Some(sink) = output.sink_for(&resource_name, generation, attempt) {
                        sink.stdout_line(Bytes::from(format!("leaf:{id}")));
                    }

                    match behavior.as_str() {
                        "ok" => Ok(()),
                        "fail" => Err(TaskError::fail(format!("{id} failed")).with_exit_code(17)),
                        "fatal" => {
                            Err(TaskError::fatal(format!("{id} is fatal")).with_exit_code(73))
                        }
                        "cancel" => Err(TaskError::Canceled),
                        other => Err(TaskError::fatal(format!(
                            "test leaf '{id}' has unknown behavior '{other}'"
                        ))),
                    }
                }
            },
        ))
    }
}

fn leaf_fields(task: &Task) -> Result<(String, String), RunnerError> {
    let TaskWorkload::Extension(workload) = task.spec().workload() else {
        return Err(RunnerError::UnsupportedWorkload {
            runner: "test-leaf".to_owned(),
            api_version: task.spec().workload().api_version().to_owned(),
            kind: task.spec().workload().kind().to_owned(),
        });
    };
    let id = string_field(workload.spec(), "id")?;
    let behavior = string_field(workload.spec(), "behavior")?;
    Ok((id, behavior))
}

fn string_field(spec: &Value, field: &str) -> Result<String, RunnerError> {
    spec.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RunnerError::InvalidSpec(format!("missing string field '{field}'")))
}

fn leaf(id: &str, behavior: &str) -> TaskWorkload {
    extension(TEST_KIND, json!({ "id": id, "behavior": behavior }))
}

fn missing_leaf(id: &str) -> TaskWorkload {
    extension(MISSING_KIND, json!({ "id": id, "behavior": "ok" }))
}

fn extension(kind: &str, spec: Value) -> TaskWorkload {
    TaskWorkload::Extension(
        ExtensionWorkload::new(TEST_API_VERSION, kind, spec)
            .expect("test extension workload must be valid"),
    )
}

fn chain_task(name: &str, chain: ChainSpec) -> Task {
    let spec = TaskSpec::builder(
        format!("{name}-slot"),
        chain
            .into_workload()
            .expect("test chain workload must serialize"),
        10_000_u64,
    )
    .build()
    .expect("test task spec must be valid");
    Task::new(name, spec).expect("test task must be valid")
}

fn chain_task_with_selector(name: &str, chain: ChainSpec, selector: LabelSelector) -> Task {
    let spec = TaskSpec::builder(
        format!("{name}-slot"),
        chain
            .into_workload()
            .expect("test chain workload must serialize"),
        10_000_u64,
    )
    .runner_selector(selector)
    .build()
    .expect("test task spec must be valid");
    Task::new(name, spec).expect("test task must be valid")
}

fn labels(key: &str, value: &str) -> Labels {
    let mut labels = Labels::new();
    labels.insert(key, value);
    labels
}

fn basic_router(
    executions: Arc<Mutex<Vec<String>>>,
    builds: Arc<Mutex<Vec<String>>>,
) -> RunnerRouter {
    let mut router = RunnerRouter::new();
    router
        .register(Arc::new(LeafRunner::new("leaf", executions, builds)))
        .expect("leaf runner registration must succeed");
    register_chain_runner(&mut router, "chain").expect("chain runner registration must succeed");
    router
}

fn build_task(router: &RunnerRouter, task: &Task) -> TaskRef {
    router.build(task).expect("chain task must build")
}

fn execution_log(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock()
        .expect("execution log mutex must not be poisoned")
        .clone()
}

#[tokio::test]
async fn success_path_runs_in_order_and_does_not_run_failure_branch() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let builds = Arc::new(Mutex::new(Vec::new()));
    let router = basic_router(Arc::clone(&executions), Arc::clone(&builds));

    let entry = ChainStep::new("entry", leaf("entry", "ok"))
        .unwrap()
        .with_on_success("success")
        .unwrap()
        .with_on_failure("failure", FailureMode::Recover)
        .unwrap();
    let chain = ChainSpec::new(
        "entry",
        vec![
            entry,
            ChainStep::new("success", leaf("success", "ok")).unwrap(),
            ChainStep::new("failure", leaf("failure", "ok")).unwrap(),
        ],
    )
    .unwrap();
    let task = chain_task("ordered", chain);

    build_task(&router, &task)
        .spawn(TaskContext::detached())
        .await
        .expect("success path must complete");

    assert_eq!(execution_log(&executions), ["leaf:entry", "leaf:success"]);
    assert_eq!(
        execution_log(&builds),
        ["leaf:entry", "leaf:success", "leaf:failure"]
    );
}

#[tokio::test]
async fn preserve_keeps_failure_while_recover_allows_successful_handler() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let builds = Arc::new(Mutex::new(Vec::new()));
    let router = basic_router(Arc::clone(&executions), builds);

    for (mode, should_recover) in [(FailureMode::Preserve, false), (FailureMode::Recover, true)] {
        let entry = ChainStep::new("entry", leaf("entry", "fail"))
            .unwrap()
            .with_on_failure("handler", mode)
            .unwrap();
        let chain = ChainSpec::new(
            "entry",
            vec![
                entry,
                ChainStep::new("handler", leaf("handler", "ok")).unwrap(),
            ],
        )
        .unwrap();
        let task = chain_task(
            if should_recover {
                "recover-chain"
            } else {
                "preserve-chain"
            },
            chain,
        );

        let result = build_task(&router, &task)
            .spawn(TaskContext::detached())
            .await;
        if should_recover {
            assert!(
                result.is_ok(),
                "recover mode must clear the handled failure"
            );
        } else {
            match result {
                Err(TaskError::Fail {
                    reason, exit_code, ..
                }) => {
                    assert_eq!(reason, "entry failed");
                    assert_eq!(exit_code, Some(17));
                }
                other => panic!("preserve mode returned an unexpected result: {other:?}"),
            }
        }
    }

    assert_eq!(
        execution_log(&executions),
        ["leaf:entry", "leaf:handler", "leaf:entry", "leaf:handler"]
    );
}

#[tokio::test]
async fn terminal_failure_preserves_exact_error_category_and_exit_code() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let router = basic_router(Arc::clone(&executions), Arc::new(Mutex::new(Vec::new())));
    let chain = ChainSpec::new(
        "terminal",
        vec![ChainStep::new("terminal", leaf("terminal", "fatal")).unwrap()],
    )
    .unwrap();
    let task = chain_task("terminal-error", chain);

    let result = build_task(&router, &task)
        .spawn(TaskContext::detached())
        .await;

    match result {
        Err(TaskError::Fatal {
            reason, exit_code, ..
        }) => {
            assert_eq!(reason, "terminal is fatal");
            assert_eq!(exit_code, Some(73));
        }
        other => panic!("chain changed the terminal task error: {other:?}"),
    }
    assert_eq!(execution_log(&executions), ["leaf:terminal"]);
}

#[tokio::test]
async fn every_spawn_restarts_the_chain_at_entry() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let router = basic_router(Arc::clone(&executions), Arc::new(Mutex::new(Vec::new())));
    let entry = ChainStep::new("entry", leaf("entry", "ok"))
        .unwrap()
        .with_on_success("next")
        .unwrap();
    let chain = ChainSpec::new(
        "entry",
        vec![entry, ChainStep::new("next", leaf("next", "ok")).unwrap()],
    )
    .unwrap();
    let task = chain_task("repeatable", chain);
    let runnable = build_task(&router, &task);

    runnable
        .spawn(TaskContext::detached())
        .await
        .expect("first outer attempt must complete");
    runnable
        .spawn(TaskContext::detached())
        .await
        .expect("second outer attempt must complete");

    assert_eq!(
        execution_log(&executions),
        ["leaf:entry", "leaf:next", "leaf:entry", "leaf:next"]
    );
}

#[tokio::test]
async fn cancellation_bypasses_failure_transition() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let router = basic_router(Arc::clone(&executions), Arc::new(Mutex::new(Vec::new())));
    let entry = ChainStep::new("entry", leaf("entry", "cancel"))
        .unwrap()
        .with_on_failure("handler", FailureMode::Recover)
        .unwrap();
    let chain = ChainSpec::new(
        "entry",
        vec![
            entry,
            ChainStep::new("handler", leaf("handler", "ok")).unwrap(),
        ],
    )
    .unwrap();
    let task = chain_task("canceled", chain);

    let result = build_task(&router, &task)
        .spawn(TaskContext::detached())
        .await;

    assert!(matches!(result, Err(TaskError::Canceled)));
    assert_eq!(execution_log(&executions), ["leaf:entry"]);
}

#[test]
fn build_compiles_every_branch_and_rejects_an_unroutable_one() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let builds = Arc::new(Mutex::new(Vec::new()));
    let router = basic_router(executions, Arc::clone(&builds));
    let entry = ChainStep::new("entry", leaf("entry", "ok"))
        .unwrap()
        .with_on_success("selected")
        .unwrap()
        .with_on_failure("unselected-missing", FailureMode::Recover)
        .unwrap();
    let chain = ChainSpec::new(
        "entry",
        vec![
            entry,
            ChainStep::new("selected", leaf("selected", "ok")).unwrap(),
            ChainStep::new("unselected-missing", missing_leaf("unselected-missing")).unwrap(),
        ],
    )
    .unwrap();
    let task = chain_task("invalid-branch", chain);

    let error = match router.build(&task) {
        Ok(_) => panic!("build unexpectedly accepted an unroutable branch"),
        Err(error) => error,
    };

    let message = error.to_string();
    assert!(message.contains("unselected-missing"), "{message}");
    assert!(message.contains("no runner"), "{message}");
    assert_eq!(execution_log(&builds), ["leaf:entry", "leaf:selected"]);
}

#[tokio::test]
async fn outer_selector_is_not_inherited_and_each_step_selector_is_local() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let builds = Arc::new(Mutex::new(Vec::new()));
    let mut router = RunnerRouter::new();
    router
        .register_with_labels(
            Arc::new(LeafRunner::new(
                "leaf-a",
                Arc::clone(&executions),
                Arc::clone(&builds),
            )),
            labels("backend", "a"),
        )
        .unwrap();
    router
        .register_with_labels(
            Arc::new(LeafRunner::new(
                "leaf-b",
                Arc::clone(&executions),
                Arc::clone(&builds),
            )),
            labels("backend", "b"),
        )
        .unwrap();
    let chain_runner = Arc::new(ChainRunner::new("chain", router.catalog()));
    router
        .register_with_labels(chain_runner, labels("role", "chain"))
        .unwrap();

    let first = ChainStep::new("first", leaf("first", "ok"))
        .unwrap()
        .with_on_success("second")
        .unwrap();
    let second = ChainStep::new("second", leaf("second", "ok"))
        .unwrap()
        .with_runner_selector(LabelSelector::from_labels(labels("backend", "b")))
        .unwrap();
    let chain = ChainSpec::new("first", vec![first, second]).unwrap();
    let task = chain_task_with_selector(
        "selector-scope",
        chain,
        LabelSelector::from_labels(labels("role", "chain")),
    );

    build_task(&router, &task)
        .spawn(TaskContext::detached())
        .await
        .expect("selector-isolated chain must complete");

    assert_eq!(execution_log(&builds), ["leaf-a:first", "leaf-b:second"]);
    assert_eq!(
        execution_log(&executions),
        ["leaf-a:first", "leaf-b:second"]
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SinkRequest {
    task_name: TaskId,
    generation: u64,
    attempt: u32,
}

#[derive(Default)]
struct RecordingOutput {
    requests: Arc<Mutex<Vec<SinkRequest>>>,
    events: Arc<Mutex<Vec<OutputEvent>>>,
}

impl RecordingOutput {
    fn requests(&self) -> Vec<SinkRequest> {
        self.requests
            .lock()
            .expect("sink request mutex must not be poisoned")
            .clone()
    }

    fn events(&self) -> Vec<OutputEvent> {
        self.events
            .lock()
            .expect("output event mutex must not be poisoned")
            .clone()
    }
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        self.requests
            .lock()
            .expect("sink request mutex must not be poisoned")
            .push(SinkRequest {
                task_name: task_name.clone(),
                generation,
                attempt,
            });
        let events = Arc::clone(&self.events);
        Some(OutputSink::new(generation, attempt, move |event| {
            events
                .lock()
                .expect("output event mutex must not be poisoned")
                .push(event);
        }))
    }
}

#[tokio::test]
async fn output_uses_one_upstream_sink_per_outer_attempt_and_one_shared_sequence() {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(RecordingOutput::default());
    let publisher: OutputPublisherHandle = recorder.clone();
    let mut router = RunnerRouter::new().with_output_publisher(publisher);
    router
        .register(Arc::new(LeafRunner::new(
            "leaf",
            executions,
            Arc::new(Mutex::new(Vec::new())),
        )))
        .unwrap();
    register_chain_runner(&mut router, "chain").unwrap();

    let entry = ChainStep::new("entry", leaf("entry", "ok"))
        .unwrap()
        .with_on_success("next")
        .unwrap();
    let chain = ChainSpec::new(
        "entry",
        vec![entry, ChainStep::new("next", leaf("next", "ok")).unwrap()],
    )
    .unwrap();
    let task = chain_task("output-chain", chain);
    let outer_name = task.name().clone();
    let generation = task.metadata().generation();
    let runnable = build_task(&router, &task);

    runnable.spawn(TaskContext::detached()).await.unwrap();
    runnable.spawn(TaskContext::detached()).await.unwrap();

    assert_eq!(
        recorder.requests(),
        [
            SinkRequest {
                task_name: outer_name.clone(),
                generation,
                attempt: 1,
            },
            SinkRequest {
                task_name: outer_name,
                generation,
                attempt: 2,
            },
        ]
    );

    let mut stdout_by_attempt = BTreeMap::<u32, Vec<(u64, Vec<u8>)>>::new();
    for event in recorder.events() {
        let OutputEvent::Chunk(chunk) = event else {
            panic!("recording publisher received a non-chunk event");
        };
        assert_eq!(chunk.generation, generation);
        assert_eq!(chunk.stream, StreamKind::Stdout);
        stdout_by_attempt
            .entry(chunk.attempt)
            .or_default()
            .push((chunk.seq, chunk.line.to_vec()));
    }

    let expected_lines = [
        b"[chain] step=entry state=started".as_slice(),
        b"leaf:entry".as_slice(),
        b"[chain] step=entry state=succeeded".as_slice(),
        b"[chain] step=next state=started".as_slice(),
        b"leaf:next".as_slice(),
        b"[chain] step=next state=succeeded".as_slice(),
    ];
    assert_eq!(stdout_by_attempt.len(), 2);
    for attempt in [1, 2] {
        let chunks = stdout_by_attempt
            .get(&attempt)
            .unwrap_or_else(|| panic!("missing output for outer attempt {attempt}"));
        assert_eq!(
            chunks.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            chunks
                .iter()
                .map(|(_, line)| line.as_slice())
                .collect::<Vec<_>>(),
            expected_lines
        );
    }
}
