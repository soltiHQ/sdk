//! # Routed task with live output
//!
//! A routed workload enters core as desired state.
//! `RunnerRouter` selects an application-provided runner by GVK and labels.
//! The runner builds a `TaskRef` and publishes attempt output through core.
//!
//! This example shows:
//!
//! - an application-owned extension workload;
//! - runner registration and selector-based routing;
//! - desired-state commit before reconciliation;
//! - `Reconciled` changing from `Unknown` to `True`;
//! - a live-only output subscription;
//! - authoritative terminal state after Taskvisor completion.
//!
//! Output is not replayed.
//! The example opens its subscription after the task starts, then releases the runner.
//!
//! Run with `cargo run -p solti-core --example routed_output`.

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::json;
use solti_core::{SupervisorApi, TaskWatchSubscription};
use solti_model::{
    ConditionStatus, ExtensionWorkload, LabelSelector, Labels, OutputEvent, Task, TaskFilter,
    TaskManifest, TaskPhase, TaskSpec, TaskWorkload, WorkloadTypeMeta,
};
use solti_runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::sync::Notify;
use tokio_stream::StreamExt;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const API_VERSION: &str = "media.example.io/v1";
const KIND: &str = "ImageResize";

const FLOW: &str = r#"
solti-core: routed workload and live output

  ExtensionWorkload(media.example.io/v1, ImageResize)
                  + runnerSelector: accelerator=cpu
                                   │ create_task
                                   ▼
                         desired Task generation 1
                         Reconciled=Unknown
                                   │ reconciliation
                                   ▼
  RunnerRouter ──► ImageResizeRunner ──► taskvisor::TaskRef
                                                │ execute
                                                ├──► OutputPublisher
                                                │          ▼
                                                │   subscribe_output
                                                │   stdout + stderr
                                                ▼
                                          Task Succeeded

  Core owns desired state, runtime supervision, output channels, and final status.
  The runner owns workload validation and TaskRef construction.
"#;

struct ImageResizeRunner {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Runner for ImageResizeRunner {
    fn name(&self) -> &str {
        "image-resize"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new(API_VERSION, KIND).expect("valid extension GVK")]
    }

    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let TaskWorkload::Extension(workload) = task.spec().workload() else {
            return Err(unsupported_workload(task.spec().workload()));
        };
        if workload.api_version() != API_VERSION || workload.kind() != KIND {
            return Err(unsupported_workload(task.spec().workload()));
        }

        let source = workload
            .spec()
            .get("source")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| RunnerError::InvalidSpec("source must be a non-empty string".into()))?
            .to_owned();
        let width = workload
            .spec()
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| RunnerError::InvalidSpec("width must be greater than zero".into()))?;

        let task_name = task.name().clone();
        let generation = task.metadata().generation();
        let output = Arc::clone(ctx.output_publisher());
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);

        Ok(TaskFn::arc(
            run_id.name().to_owned(),
            move |_ctx: TaskContext| {
                let task_name = task_name.clone();
                let output = Arc::clone(&output);
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let source = source.clone();

                async move {
                    started.notify_one();
                    release.notified().await;

                    let sink = output
                        .sink_for(&task_name, generation, 1)
                        .ok_or_else(|| TaskError::fatal("core output channel is unavailable"))?;
                    sink.stdout_line(Bytes::from(format!(
                        "loading {source} for generation {generation}"
                    )));
                    sink.stderr_line(Bytes::from_static(
                        b"example diagnostic: CPU backend selected",
                    ));
                    sink.stdout_line(Bytes::from(format!("resized {source} to {width}px")));
                    Ok::<(), TaskError>(())
                }
            },
        ))
    }
}

fn unsupported_workload(workload: &TaskWorkload) -> RunnerError {
    RunnerError::UnsupportedWorkload {
        runner: "image-resize".into(),
        api_version: workload.api_version().into(),
        kind: workload.kind().into(),
    }
}

fn runner_labels() -> Labels {
    let mut labels = Labels::new();
    labels.insert("accelerator", "cpu");
    labels
}

fn manifest() -> ExampleResult<TaskManifest> {
    let workload = TaskWorkload::Extension(ExtensionWorkload::new(
        API_VERSION,
        KIND,
        json!({
            "source": "cover.png",
            "width": 1280
        }),
    )?);
    let spec = TaskSpec::builder("image-resize", workload, 30_000_u64)
        .runner_selector(LabelSelector::from_labels(runner_labels()))
        .build()?;
    Ok(TaskManifest::new("resize-cover", spec)?)
}

async fn wait_for_task(
    watch: &mut TaskWatchSubscription,
    predicate: impl Fn(&Task) -> bool,
) -> ExampleResult<Task> {
    while let Some(event) = watch.next().await {
        let event = event?;
        if predicate(event.object()) {
            return Ok(event.into_object());
        }
    }

    Err(io::Error::other("task watch closed before the expected transition").into())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Follow one custom workload from desired-state commit through routing, live output, and terminal state."
    );

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut router = RunnerRouter::new();
    router.register_with_labels(
        Arc::new(ImageResizeRunner {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }),
        runner_labels(),
    )?;
    println!("[setup] Registered image-resize for {API_VERSION}/{KIND} with accelerator=cpu.");

    let api = SupervisorApi::builder(router).start().await?;
    let mut watch = api.watch_tasks(&TaskFilter::new(), Some("0"))?;
    let committed = api.create_task(manifest()?).await?;
    let name = committed.name().clone();
    println!(
        "[create] Desired state committed: task={}, generation={}, Reconciled={:?}.",
        name,
        committed.metadata().generation(),
        committed.status().reconciled().status(),
    );
    assert_eq!(
        committed.status().reconciled().status(),
        ConditionStatus::Unknown
    );

    started.notified().await;
    let running = wait_for_task(&mut watch, |task| {
        task.name() == &name
            && task.status().reconciled().status() == ConditionStatus::True
            && task.phase() == &TaskPhase::Running
    })
    .await?;
    println!(
        "[reconcile] Runner accepted generation {}; task phase is {}.",
        running.metadata().generation(),
        running.phase(),
    );

    let mut output = api
        .subscribe_output(&name)
        .ok_or_else(|| io::Error::other("task output channel is unavailable"))?;
    println!("[output] Subscribed after execution started; earlier events are not replayed.");
    release.notify_one();

    let mut chunks = 0;
    while let Some(event) = output.next().await {
        match event {
            OutputEvent::Chunk(chunk) => {
                let line = String::from_utf8_lossy(&chunk.line);
                println!(
                    "[output] generation={} attempt={} stream={:?} seq={} line={line:?}.",
                    chunk.generation, chunk.attempt, chunk.stream, chunk.seq,
                );
                chunks += 1;
            }
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                ..
            } => {
                println!(
                    "[output] RunFinished: generation={generation}, attempt={attempt}, exitCode={exit_code:?}."
                );
                break;
            }
            OutputEvent::RunStarted {
                generation,
                attempt,
                ..
            } => {
                println!("[output] RunStarted: generation={generation}, attempt={attempt}.");
            }
            OutputEvent::Lagged { skipped } => {
                println!("[output] Lagged: skipped={skipped}.");
            }
            _ => {}
        }
    }
    assert_eq!(chunks, 3);

    let finished = wait_for_task(&mut watch, |task| {
        task.name() == &name && task.phase() == &TaskPhase::Succeeded
    })
    .await?;
    println!(
        "[complete] Authoritative task phase is {}; retained runs={}.",
        finished.phase(),
        api.list_task_runs(&name).len(),
    );

    api.shutdown().await?;
    println!(
        "\nResult: core routed the extension workload, supervised its TaskRef, streamed three live chunks, and finalized the resource as Succeeded."
    );
    Ok(())
}
