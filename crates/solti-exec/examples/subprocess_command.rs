//! # Subprocess command
//!
//! A `Subprocess` workload becomes one reusable Taskvisor task.
//! Each attempt starts a new operating-system process.
//!
//! This example shows:
//!
//! - runner registration and GVK routing;
//! - cleared environment with task and runner values;
//! - runner values overriding task values;
//! - an allowed and pinned working directory;
//! - stdout and stderr reaching an application-owned output publisher;
//! - one complete process attempt.
//!
//! Run with `cargo run -p solti-exec --example subprocess_command --features subprocess`.

use std::sync::{Arc, Mutex};

use solti_exec::subprocess::{
    CwdPolicy, EnvPolicy, SubprocessBackendConfig, register_subprocess_runner_with_backend,
};
use solti_model::{
    Flag, OutputEvent, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskSpec,
    TaskWorkload,
};
use solti_runner::{
    BuildContext, OutputPublisher, OutputPublisherHandle, OutputSink, RunnerEnv, RunnerRouter,
};
use taskvisor::TaskContext;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-exec: one subprocess command attempt

  Task { workload: Subprocess::Command }
      │ task env + cwd + failOnNonZero
      ▼
  RunnerRouter ──► SubprocessRunner ──► reusable TaskRef
                                              │ spawn attempt 1
  SubprocessBackendConfig                     ▼
      ├── EnvPolicy::Clear ───────────► /bin/sh
      ├── CwdPolicy::Roots ───────────► pinned working directory
      └── OutputPublisher ◄──────────── stdout + stderr
                                              ▼
                                      exit + reap + cleanup

  Building performs no process I/O.
  The attempt owns the child and its output readers.
"#;

#[derive(Default)]
struct RecordingOutput {
    events: Arc<Mutex<Vec<OutputEvent>>>,
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        println!(
            "[output] Opened a sink for task={task_name}, generation={generation}, attempt={attempt}."
        );
        let events = Arc::clone(&self.events);
        Some(OutputSink::new(generation, attempt, move |event| {
            events
                .lock()
                .expect("output recorder lock must not be poisoned")
                .push(event);
        }))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Resolve one Subprocess resource into a process attempt with explicit environment, cwd, and output boundaries."
    );

    let workdir = tempfile::tempdir()?;
    let canonical_workdir = workdir.path().canonicalize()?;
    println!(
        "[setup/cwd] Only {} is accepted as a task working-directory root.",
        canonical_workdir.display(),
    );

    let output = Arc::new(RecordingOutput::default());
    let output_handle: OutputPublisherHandle = output.clone();
    let mut runner_env = RunnerEnv::new();
    runner_env.push("SHARED", "from-runner");
    let context = BuildContext::default()
        .with_env(runner_env)
        .with_output_publisher(output_handle);
    let mut router = RunnerRouter::new().with_context(context);

    let backend = SubprocessBackendConfig::new()
        .with_env_policy(EnvPolicy::Clear)
        .with_cwd_policy(CwdPolicy::Roots(vec![canonical_workdir.clone()]));
    register_subprocess_runner_with_backend(&mut router, "local", backend)?;
    println!("[setup/runner] Registered runner local with a cleared child environment.");

    let mut task_env = TaskEnv::new();
    task_env.push("TASK_VALUE", "from-task");
    task_env.push("SHARED", "from-task");
    let script = r#"printf 'cwd=%s\n' "$PWD"
printf 'task=%s\n' "$TASK_VALUE"
printf 'shared=%s\n' "$SHARED"
printf 'diagnostic=example\n' >&2"#;
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
        },
        task_env,
        Some(canonical_workdir.clone()),
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("jobs", workload, 5_000_u64).build()?;
    let task = Task::new("inspect-process-boundary", spec)?;
    println!("[setup/task] Task sets TASK_VALUE and SHARED; runner SHARED must win.");

    let task_ref = router.build(&task).await?;
    println!(
        "[build] Router selected local and built {}; no child exists yet.",
        task_ref.name(),
    );
    task_ref.spawn(TaskContext::detached()).await?;
    println!("[attempt] The child exited successfully and was reaped.");

    let events = output
        .events
        .lock()
        .expect("output recorder lock must not be poisoned");
    let mut lines = Vec::new();
    println!("[output] Published workload lines:");
    for event in &*events {
        let OutputEvent::Chunk(chunk) = event else {
            continue;
        };
        let line = String::from_utf8_lossy(&chunk.line).into_owned();
        println!(
            "      attempt={} stream={:?} seq={} line={line:?}",
            chunk.attempt, chunk.stream, chunk.seq,
        );
        lines.push(line);
    }
    assert!(lines.iter().any(|line| line == "task=from-task"));
    assert!(lines.iter().any(|line| line == "shared=from-runner"));
    assert!(
        lines
            .iter()
            .any(|line| line == &format!("cwd={}", canonical_workdir.display()))
    );
    assert!(lines.iter().any(|line| line == "diagnostic=example"));

    println!(
        "\nResult: the runner applied its environment and cwd policy, executed one child, published both streams, and completed cleanup."
    );
    Ok(())
}
