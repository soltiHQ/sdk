//! # Runner build context
//!
//! `BuildContext` carries dependencies shared by every registered runner.
//! A runner reads them while it converts a resource into a `TaskRef`.
//!
//! This example shows:
//!
//! - task and runner environment merging;
//! - runner values overriding task values;
//! - a custom metrics backend receiving runner and error labels;
//! - an output publisher creating one attempt-scoped sink;
//! - cloned sinks sharing separate stdout and stderr sequences.
//!
//! Output callbacks run synchronously.
//! The in-memory recorder is suitable for this example only.
//! Production callbacks should forward events without blocking runner execution.
//!
//! Run with `cargo run -p solti-runner --example build_context`.

use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use solti_model::{OutputEvent, StreamKind, TaskEnv, TaskId};
use solti_runner::{
    BuildContext, MetricsBackend, MetricsHandle, OutputPublisher, OutputPublisherHandle,
    OutputSink, RunnerEnv, RunnerErrorKind, RunnerType, merge_env,
};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-runner: build context ports

  TaskEnv ─────────────────────────────┐
                                       ├──► merge_env ──► sorted process environment
  BuildContext                         │
      ├── RunnerEnv ───────────────────┘
      │       └── runner values override task values; last duplicate wins
      │
      ├── MetricsHandle ──► MetricsBackend
      │                         └──► runner type + error kind
      │
      └── OutputPublisher ──► OutputSink(task, generation, attempt)
                                  ├──► stdout chunks: seq 0, 1, ...
                                  └──► stderr chunks: seq 0, 1, ...

  A runner receives these ports while it builds a task.
  The composition layer supplies their concrete implementations.
"#;

#[derive(Default)]
struct RecordingMetrics {
    errors: Mutex<Vec<(String, String)>>,
}

impl MetricsBackend for RecordingMetrics {
    fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind) {
        self.errors
            .lock()
            .expect("metrics recorder lock must not be poisoned")
            .push((
                runner_type.as_label().to_owned(),
                error_kind.as_label().to_owned(),
            ));
    }
}

#[derive(Default)]
struct RecordingOutput {
    events: Arc<Mutex<Vec<OutputEvent>>>,
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        println!(
            "[output/publisher] Created a sink for task={task_name}, generation={generation}, attempt={attempt}."
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

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Inject shared environment, metrics, and output behavior without coupling a runner to their implementations."
    );

    println!("[environment/setup] Task values: JOB=thumbnail, PATH=/task/bin.");
    let mut task_env = TaskEnv::new();
    task_env.push("JOB", "thumbnail");
    task_env.push("PATH", "/task/bin");

    let mut runner_env = RunnerEnv::new();
    runner_env.push("PATH", "/runner/first");
    runner_env.push("PATH", "/runner/bin");
    runner_env.push("RUNNER_MODE", "isolated");
    println!(
        "[environment/setup] Runner values: PATH=/runner/first, PATH=/runner/bin, RUNNER_MODE=isolated."
    );

    println!("[context] Install application-owned metrics and output implementations.");
    let metrics = Arc::new(RecordingMetrics::default());
    let metrics_handle: MetricsHandle = metrics.clone();
    let output = Arc::new(RecordingOutput::default());
    let output_handle: OutputPublisherHandle = output.clone();
    let context =
        BuildContext::new(runner_env, metrics_handle).with_output_publisher(output_handle);

    let merged = merge_env(&task_env, context.env());
    println!("[environment/result] The sorted merged environment is:");
    for (key, value) in &merged {
        println!("      {key}={value}");
    }
    assert_eq!(merged["PATH"], "/runner/bin");
    assert_eq!(merged["JOB"], "thumbnail");
    assert_eq!(merged["RUNNER_MODE"], "isolated");
    println!(
        "[environment/result] PATH=/runner/bin because runner values override task values and the last duplicate wins."
    );

    context.metrics().record_runner_error(
        RunnerType::Custom("image-resize".into()),
        RunnerErrorKind::Custom("decoder_unavailable".into()),
    );
    let recorded_metrics = metrics
        .errors
        .lock()
        .expect("metrics recorder lock must not be poisoned");
    for (runner, error) in &*recorded_metrics {
        println!(
            "[metrics] The backend received runner and error labels: runner={runner}, error={error}."
        );
    }
    assert_eq!(
        recorded_metrics.as_slice(),
        &[("image-resize".into(), "decoder_unavailable".into())],
    );
    drop(recorded_metrics);

    let task_name = TaskId::new("resize-cover")?;
    let sink = context
        .output_publisher()
        .sink_for(&task_name, 4, 2)
        .ok_or_else(|| io::Error::other("output publisher disabled the attempt"))?;
    let cloned_sink = sink.clone();
    sink.stdout_line(Bytes::from_static(b"starting"));
    cloned_sink.stdout_line(Bytes::from_static(b"finished"));
    sink.stderr_line(Bytes::from_static(b"warning"));

    let recorded_output = output
        .events
        .lock()
        .expect("output recorder lock must not be poisoned");
    println!("[output/events] Cloned sinks share one sequence per stream:");
    let mut stdout_sequences = Vec::new();
    let mut stderr_sequences = Vec::new();
    for event in &*recorded_output {
        let OutputEvent::Chunk(chunk) = event else {
            return Err(io::Error::other("OutputSink emitted a non-chunk event").into());
        };
        assert_eq!(chunk.generation, 4);
        assert_eq!(chunk.attempt, 2);
        match chunk.stream {
            StreamKind::Stdout => stdout_sequences.push(chunk.seq),
            StreamKind::Stderr => stderr_sequences.push(chunk.seq),
        }
        println!(
            "      generation={} attempt={} stream={:?} seq={} line={:?}",
            chunk.generation,
            chunk.attempt,
            chunk.stream,
            chunk.seq,
            String::from_utf8_lossy(&chunk.line),
        );
    }
    assert_eq!(stdout_sequences, [0, 1]);
    assert_eq!(stderr_sequences, [0]);

    println!(
        "\nResult: environment precedence is explicit; application-owned adapters received metrics and output."
    );

    Ok(())
}
