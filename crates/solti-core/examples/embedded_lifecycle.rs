//! # Embedded task lifecycle
//!
//! An embedded workload keeps its implementation inside the application.
//! The manifest carries an implementation revision.
//! The matching `TaskRef` is submitted through the embedded API.
//!
//! This example shows:
//!
//! - desired-state commit before runtime reconciliation;
//! - `Reconciled` changing from `Unknown` to `True`;
//! - a revision change advancing `metadata.generation`;
//! - cooperative cancellation of the previous runtime;
//! - retained attempt history for both generations.
//!
//! Apply uses latest-wins semantics.
//! It does not provide a staged rollout or an availability guarantee.
//! Attempt history comes from Taskvisor's best-effort event stream.
//!
//! Run with `cargo run -p solti-core --example embedded_lifecycle`.

use std::io;
use std::sync::Arc;

use solti_core::{SupervisorApi, TaskWatchSubscription};
use solti_model::{
    ConditionStatus, EmbeddedSpec, Task, TaskFilter, TaskManifest, TaskPhase, TaskSpec,
    TaskWorkload,
};
use solti_runner::RunnerRouter;
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::sync::Notify;
use tokio_stream::StreamExt;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Builds one embedded desired-state revision.
fn manifest(revision: &str) -> ExampleResult<TaskManifest> {
    let workload = TaskWorkload::Embedded(EmbeddedSpec::new(revision)?);
    let spec = TaskSpec::builder("maintenance", workload, 30_000_u64).build()?;
    Ok(TaskManifest::new("cache-refresh", spec)?)
}

/// Creates a task that finishes on release and reports cooperative cancellation.
fn controlled_task(
    name: &'static str,
    started: Arc<Notify>,
    release: Arc<Notify>,
    cancelled: Arc<Notify>,
) -> TaskRef {
    TaskFn::arc(name, move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let cancelled = Arc::clone(&cancelled);

        async move {
            started.notify_one();
            tokio::select! {
                _ = release.notified() => Ok(()),
                _ = ctx.cancelled() => {
                    cancelled.notify_one();
                    Err(TaskError::Canceled)
                }
            }
        }
    })
}

/// Waits for a task watch event that satisfies `predicate`.
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
    let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;
    let mut watch = api.watch_tasks(&TaskFilter::new(), Some("0"))?;

    let v1_started = Arc::new(Notify::new());
    let v1_release = Arc::new(Notify::new());
    let v1_cancelled = Arc::new(Notify::new());
    let committed = api
        .create_embedded_task(
            manifest("implementation-v1")?,
            controlled_task(
                "cache-refresh-v1",
                Arc::clone(&v1_started),
                v1_release,
                Arc::clone(&v1_cancelled),
            ),
        )
        .await?;
    let name = committed.name().clone();

    println!(
        "commit: generation={}, observedGeneration={}, Reconciled={:?}",
        committed.metadata().generation(),
        committed.status().observed_generation(),
        committed.status().reconciled().status(),
    );
    assert_eq!(
        committed.status().reconciled().status(),
        ConditionStatus::Unknown
    );

    v1_started.notified().await;
    let running_v1 = wait_for_task(&mut watch, |task| {
        task.name() == &name
            && task.metadata().generation() == 1
            && task.status().reconciled().status() == ConditionStatus::True
            && task.phase() == &TaskPhase::Running
    })
    .await?;
    println!(
        "runtime: generation={} phase={} Reconciled={:?}",
        running_v1.metadata().generation(),
        running_v1.phase(),
        running_v1.status().reconciled().status(),
    );

    let v2_started = Arc::new(Notify::new());
    let v2_release = Arc::new(Notify::new());
    let v2_cancelled = Arc::new(Notify::new());
    let applied = api
        .apply_embedded_task(
            manifest("implementation-v2")?,
            controlled_task(
                "cache-refresh-v2",
                Arc::clone(&v2_started),
                Arc::clone(&v2_release),
                v2_cancelled,
            ),
        )
        .await?;

    println!(
        "apply: generation={} observedGeneration={} Reconciled={:?}",
        applied.metadata().generation(),
        applied.status().observed_generation(),
        applied.status().reconciled().status(),
    );
    assert_eq!(applied.metadata().generation(), 2);
    assert_eq!(
        applied.status().reconciled().status(),
        ConditionStatus::Unknown
    );

    v1_cancelled.notified().await;
    println!("generation 1 observed cooperative cancellation");

    v2_started.notified().await;
    let running_v2 = wait_for_task(&mut watch, |task| {
        task.name() == &name
            && task.metadata().generation() == 2
            && task.status().reconciled().status() == ConditionStatus::True
            && task.phase() == &TaskPhase::Running
    })
    .await?;
    println!(
        "runtime: generation={} phase={} Reconciled={:?}",
        running_v2.metadata().generation(),
        running_v2.phase(),
        running_v2.status().reconciled().status(),
    );

    v2_release.notify_one();
    let finished = wait_for_task(&mut watch, |task| {
        task.name() == &name
            && task.metadata().generation() == 2
            && task.phase() == &TaskPhase::Succeeded
    })
    .await?;
    println!(
        "finished: generation={} phase={}",
        finished.metadata().generation(),
        finished.phase(),
    );

    println!("runs:");
    for run in api.list_task_runs(&name) {
        println!(
            "  generation={} attempt={} phase={}",
            run.generation(),
            run.attempt(),
            run.phase(),
        );
    }

    api.shutdown().await?;
    Ok(())
}
