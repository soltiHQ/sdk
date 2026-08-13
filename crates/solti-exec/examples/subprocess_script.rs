//! # Subprocess script
//!
//! Script mode carries a base64 body and an explicit interpreter.
//! The runner decodes the body while it builds the task.
//! Every attempt creates fresh script transport and removes it after execution.
//!
//! This example shows:
//!
//! - a caller-provided interpreter and script body;
//! - build-time base64 decoding;
//! - one reusable Taskvisor task producing two attempts;
//! - fresh attempt numbers in published output;
//! - script arguments and environment values.
//!
//! Run with `cargo run -p solti-exec --example subprocess_script --features subprocess`.

use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    Flag, OutputEvent, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskSpec,
    TaskWorkload,
};
use solti_runner::{OutputPublisher, OutputPublisherHandle, OutputSink, RunnerRouter};
use taskvisor::TaskContext;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-exec: reusable script workload

  interpreter + base64 body + args + env
                    ▼
       SubprocessRunner::build_task
           └──► decode and validate body
                    ▼
              reusable TaskRef
                 ┌──┴──────────────────────────┐
                 │ spawn attempt 1             │ spawn attempt 2
                 ▼                             ▼
       fresh script transport        fresh script transport
                 │                             │
                 └──► /bin/sh ◄────────────────┘
                          ├──► OutputSink(generation, attempt)
                          └──► reap + remove transport

  The script body is reusable.
  Its operating-system transport is attempt-scoped.
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
        "[purpose] Build one script workload once, then execute it through two independent attempt scopes."
    );

    let body = b"printf 'argument=%s mode=%s\\n' \"$1\" \"$MODE\"\n";
    let encoded = BASE64.encode(body);
    println!(
        "[setup/script] Encoded {} script bytes as {} base64 characters.",
        body.len(),
        encoded.len(),
    );

    let mut env = TaskEnv::new();
    env.push("MODE", "script");
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Script {
            interpreter: "/bin/sh".into(),
            body: encoded,
            args: vec!["payload".into()],
        },
        env,
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("scripts", workload, 5_000_u64).build()?;
    let task = Task::new("render-script", spec)?;

    let output = Arc::new(RecordingOutput::default());
    let output_handle: OutputPublisherHandle = output.clone();
    let mut router = RunnerRouter::new().with_output_publisher(output_handle);
    register_subprocess_runner(&mut router, "shell")?;

    let task_ref = router.build(&task).await?;
    println!(
        "[build] Decoded the body and built {}; no script transport exists between attempts.",
        task_ref.name(),
    );

    task_ref.spawn(TaskContext::detached()).await?;
    println!(
        "[attempt/1] Created transport, ran /bin/sh, reaped the child, and removed transport."
    );
    task_ref.spawn(TaskContext::detached()).await?;
    println!("[attempt/2] Repeated the same lifecycle with a fresh attempt scope.");

    let events = output
        .events
        .lock()
        .expect("output recorder lock must not be poisoned");
    let mut observed_attempts = Vec::new();
    println!("[output] Published script lines:");
    for event in &*events {
        let OutputEvent::Chunk(chunk) = event else {
            continue;
        };
        let line = String::from_utf8_lossy(&chunk.line);
        println!(
            "      generation={} attempt={} stream={:?} seq={} line={line:?}",
            chunk.generation, chunk.attempt, chunk.stream, chunk.seq,
        );
        assert_eq!(line, "argument=payload mode=script");
        observed_attempts.push(chunk.attempt);
    }
    assert_eq!(observed_attempts, [1, 2]);

    println!(
        "\nResult: one built task produced two isolated script attempts with independent output identity and cleanup."
    );
    Ok(())
}
