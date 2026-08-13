//! # Custom container engine
//!
//! `ContainerRunner` owns the engine-neutral attempt lifecycle.
//! A final binary supplies an implementation of `ContainerEngine`.
//!
//! This example shows:
//!
//! - an explicit engine probe;
//! - a custom engine receiving one immutable `ContainerRequest`;
//! - runner and task environment merging;
//! - low-level process policy reaching the engine boundary;
//! - build without engine I/O;
//! - create, output, start, wait, and cleanup ordering.
//!
//! The engine is an in-memory teaching adapter.
//! It demonstrates the contract but does not create a real container.
//!
//! Run with `cargo run -p solti-exec --example custom_container_engine --features container`.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use solti_exec::container::{
    ContainerAttempt, ContainerEngine, ContainerEngineError, ContainerEngineInfo,
    ContainerExitStatus, ContainerOutput, ContainerProcessPolicy, ContainerRequest,
    ContainerRunnerConfig, register_container_runner_with_config,
};
use solti_exec::isolation::{CgroupLimits, ProcessCredentials, RlimitConfig, SeccompPolicy};
use solti_model::{ContainerSpec, OutputEvent, Task, TaskEnv, TaskId, TaskSpec, TaskWorkload};
use solti_runner::{
    BuildContext, OutputPublisher, OutputPublisherHandle, OutputSink, RunnerEnv, RunnerRouter,
};
use taskvisor::TaskContext;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-exec: engine-neutral container attempt

  final binary ──► ContainerEngine::probe()

  Task { workload: Container }     ContainerRunnerConfig
      │ image + command + env          └── process policy
      └──────────────────┬────────────────────┘
                         ▼
                  ContainerRunner
                         │ build: no engine I/O
                         ▼
                 reusable TaskRef
                         │ spawn
                         ▼
                  ContainerRequest
                         ▼
  custom engine: create stopped attempt ──► take stdout/stderr
                         │                  └──► OutputPublisher
                         ▼
                       start ──► wait ──► cleanup

  The runner owns lifecycle order.
  The engine owns image resolution and attempt resources.
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

struct TeachingEngine {
    calls: Arc<Mutex<Vec<String>>>,
}

impl TeachingEngine {
    fn new() -> (Arc<Self>, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
            }),
            calls,
        )
    }
}

#[async_trait]
impl ContainerEngine for TeachingEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("probe".into());
        Ok(ContainerEngineInfo::new("teaching-engine", "1"))
    }

    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        let policy = request.process_policy();
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push(format!(
                "create: id={}, image={}, command={:?}, args={:?}, env={:?}, noNewPrivileges={}, capabilityCount={}, memory={:?}",
                request.attempt_id(),
                request.image(),
                request.command(),
                request.args(),
                request.env(),
                policy.no_new_privileges(),
                policy.capabilities().map_or(0, <[solti_exec::isolation::LinuxCapability]>::len),
                policy.resources().and_then(|limits| limits.memory),
            ));
        Ok(Box::new(TeachingAttempt {
            calls: Arc::clone(&self.calls),
            stdout: Some(Box::pin(Cursor::new(b"container stdout\n".to_vec()))),
            stderr: Some(Box::pin(Cursor::new(b"container stderr\n".to_vec()))),
        }))
    }
}

struct TeachingAttempt {
    calls: Arc<Mutex<Vec<String>>>,
    stdout: Option<ContainerOutput>,
    stderr: Option<ContainerOutput>,
}

#[async_trait]
impl ContainerAttempt for TeachingAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("take_stdout".into());
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("take_stderr".into());
        self.stderr.take()
    }

    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("start".into());
        Ok(())
    }

    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("wait".into());
        Ok(ContainerExitStatus::new(0))
    }

    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("terminate".into());
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .push("cleanup".into());
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Show the exact contract a custom container adapter receives from ContainerRunner."
    );

    let (engine, calls) = TeachingEngine::new();
    let info = engine.probe().await?;
    println!(
        "[probe] Final binary accepted engine={} version={}.",
        info.name(),
        info.version(),
    );

    let policy = ContainerProcessPolicy::new()
        .with_rlimits(RlimitConfig {
            max_open_files: Some(128),
            max_file_size_bytes: None,
            disable_core_dumps: true,
        })
        .with_resources(CgroupLimits {
            cpu: None,
            memory: Some(64 * 1024 * 1024),
            pids: Some(32),
        })
        .with_credentials(ProcessCredentials::new(1000, 1000))
        .with_capabilities([])
        .with_no_new_privileges(true)
        .with_umask(0o077)
        .with_seccomp(SeccompPolicy::DenyHostControl);
    let runner_config = ContainerRunnerConfig::new().with_process_policy(policy);
    println!(
        "[policy] Requested UID:GID 1000:1000, no capabilities, noNewPrivileges, rlimits, resources, umask, and seccomp intent."
    );

    let output = Arc::new(RecordingOutput::default());
    let output_handle: OutputPublisherHandle = output.clone();
    let mut runner_env = RunnerEnv::new();
    runner_env.push("SHARED", "from-runner");
    let context = BuildContext::default()
        .with_env(runner_env)
        .with_output_publisher(output_handle);
    let mut router = RunnerRouter::new().with_context(context);
    let engine_handle: Arc<dyn ContainerEngine> = engine;
    register_container_runner_with_config(&mut router, "teaching", engine_handle, runner_config)?;

    let mut task_env = TaskEnv::new();
    task_env.push("TASK_VALUE", "from-task");
    task_env.push("SHARED", "from-task");
    let workload = TaskWorkload::Container(ContainerSpec::new(
        "registry.example/demo-worker:1".into(),
        Some(vec!["demo-worker".into()]),
        vec!["--once".into()],
        task_env,
    ));
    let spec = TaskSpec::builder("containers", workload, 30_000_u64).build()?;
    let task = Task::new("demo-container", spec)?;

    let calls_before_build = calls
        .lock()
        .expect("engine recorder lock must not be poisoned")
        .len();
    let task_ref = router.build(&task).await?;
    assert_eq!(
        calls
            .lock()
            .expect("engine recorder lock must not be poisoned")
            .len(),
        calls_before_build,
    );
    println!(
        "[build] Built {}; engine call count remained {calls_before_build}.",
        task_ref.name(),
    );

    task_ref.spawn(TaskContext::detached()).await?;
    let recorded_calls = calls
        .lock()
        .expect("engine recorder lock must not be poisoned");
    println!("[engine] Observed lifecycle calls:");
    for call in &*recorded_calls {
        println!("      {call}");
    }
    assert_eq!(recorded_calls.first().map(String::as_str), Some("probe"));
    assert!(recorded_calls[1].contains("SHARED\": \"from-runner"));
    assert_eq!(
        recorded_calls[2..],
        ["take_stdout", "take_stderr", "start", "wait", "cleanup"],
    );
    drop(recorded_calls);

    let events = output
        .events
        .lock()
        .expect("output recorder lock must not be poisoned");
    println!("[output] Runner published engine streams:");
    for event in &*events {
        let OutputEvent::Chunk(chunk) = event else {
            continue;
        };
        println!(
            "      attempt={} stream={:?} seq={} line={:?}",
            chunk.attempt,
            chunk.stream,
            chunk.seq,
            String::from_utf8_lossy(&chunk.line),
        );
    }
    assert_eq!(events.len(), 2);

    println!(
        "\nResult: ContainerRunner owned lifecycle and output; the adapter received all engine-specific inputs and the process policy it must enforce."
    );
    Ok(())
}
