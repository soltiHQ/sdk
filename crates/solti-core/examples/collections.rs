//! # Task collections
//!
//! `TaskQuery` creates a snapshot-consistent task list.
//! A continuation reads the same snapshot after live state changes.
//!
//! `watch_tasks` follows changes after an opaque resource version.
//! Watch event kinds are relative to the filter:
//!
//! - entering the filter emits `Added`;
//! - changing while still visible emits `Modified`;
//! - leaving the filter emits `Deleted`.
//!
//! Resource versions belong to one in-memory `TaskState` incarnation.
//! Treat them as opaque values and pass them back unchanged.
//!
//! Run with `cargo run -p solti-core --example collections`.

use std::io;

use solti_core::{SupervisorApi, TaskWatchSubscription};
use solti_model::{
    EmbeddedSpec, LabelSelector, Labels, Task, TaskFilter, TaskId, TaskManifest, TaskQuery,
    TaskSpec, TaskWatchEvent, TaskWorkload,
};
use solti_runner::RunnerRouter;
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio_stream::StreamExt;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Creates labels used by the watch scenario.
fn labels(environment: &str, revision: Option<&str>) -> Labels {
    let mut labels = Labels::new();
    labels.insert("environment", environment);
    if let Some(revision) = revision {
        labels.insert("revision", revision);
    }
    labels
}

/// Builds one embedded manifest with a task-specific slot.
fn manifest(name: &str, labels: Labels) -> ExampleResult<TaskManifest> {
    let workload = TaskWorkload::Embedded(EmbeddedSpec::new("collections-v1")?);
    let spec = TaskSpec::builder(name, workload, 30_000_u64).build()?;
    Ok(TaskManifest::new(name, spec)?.with_labels(labels)?)
}

/// Creates an embedded task that completes at once.
fn immediate_task(name: impl Into<String>) -> TaskRef {
    TaskFn::arc(name.into(), |_ctx: TaskContext| async move {
        Ok::<(), TaskError>(())
    })
}

/// Creates one retained resource through the public supervisor API.
async fn create_task(api: &SupervisorApi, name: &str, labels: Labels) -> ExampleResult<Task> {
    Ok(api
        .create_embedded_task(
            manifest(name, labels)?,
            immediate_task(format!("{name}-runtime")),
        )
        .await?)
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

/// Reads one required event from a watch.
async fn next_event(watch: &mut TaskWatchSubscription) -> ExampleResult<TaskWatchEvent> {
    match watch.next().await {
        Some(event) => Ok(event?),
        None => Err(io::Error::other("task watch closed before the expected event").into()),
    }
}

/// Checks and prints one filter-relative watch event.
fn show_event(expected: &str, event: TaskWatchEvent) -> ExampleResult {
    let (actual, task) = match event {
        TaskWatchEvent::Added(task) => ("Added", task),
        TaskWatchEvent::Modified(task) => ("Modified", task),
        TaskWatchEvent::Deleted(task) => ("Deleted", task),
    };
    if actual != expected || task.name().as_str() != "watched" {
        return Err(io::Error::other(format!(
            "expected {expected} for watched, got {actual} for {}",
            task.name()
        ))
        .into());
    }

    println!(
        "watch: {actual} {} at {}",
        task.name(),
        task.metadata().resource_version(),
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;

    for name in ["alpha", "beta", "gamma"] {
        create_task(&api, name, Labels::new()).await?;
    }

    let query = TaskQuery::new().with_limit(2);
    let first = api.query_tasks(&query)?;
    let first_names: Vec<_> = first
        .items
        .iter()
        .map(|task| task.name().as_str())
        .collect();
    println!(
        "page 1 at {}: {first_names:?}; remaining={}",
        first.resource_version, first.remaining_item_count,
    );

    let continuation = first
        .continuation
        .ok_or_else(|| io::Error::other("first page did not return a continuation"))?;
    let snapshot_version = first.resource_version;

    api.delete_task(&TaskId::new("gamma")?).await?;
    println!("live state: gamma deleted");

    let second = api.query_tasks(
        &TaskQuery::new()
            .with_limit(2)
            .with_continuation(continuation),
    )?;
    let second_names: Vec<_> = second
        .items
        .iter()
        .map(|task| task.name().as_str())
        .collect();
    println!("page 2 at {}: {second_names:?}", second.resource_version,);
    assert_eq!(second.resource_version, snapshot_version);
    assert_eq!(second_names, ["gamma"]);

    let before_create = api.query_tasks(&TaskQuery::new())?.resource_version;
    let mut completion_watch = api.watch_tasks(&TaskFilter::new(), Some(before_create.as_str()))?;
    let watched_runtime = immediate_task("watched-runtime");
    let watched = api
        .create_embedded_task(
            manifest("watched", labels("development", None))?,
            watched_runtime.clone(),
        )
        .await?;
    wait_for_task(&mut completion_watch, |task| {
        task.name() == watched.name() && task.phase().is_terminal()
    })
    .await?;
    drop(completion_watch);

    let baseline = api.query_tasks(&TaskQuery::new())?.resource_version;
    let selector = "environment=production".parse::<LabelSelector>()?;
    let filter = TaskFilter::new().with_label_selector(selector)?;
    let mut watch = api.watch_tasks(&filter, Some(baseline.as_str()))?;

    let current = api
        .get_task(watched.name())
        .ok_or_else(|| io::Error::other("watched task disappeared"))?;
    api.apply_embedded_task(
        TaskManifest::from(current).with_labels(labels("production", None))?,
        watched_runtime.clone(),
    )
    .await?;
    show_event("Added", next_event(&mut watch).await?)?;

    let current = api
        .get_task(watched.name())
        .ok_or_else(|| io::Error::other("watched task disappeared"))?;
    api.apply_embedded_task(
        TaskManifest::from(current).with_labels(labels("production", Some("2")))?,
        watched_runtime.clone(),
    )
    .await?;
    show_event("Modified", next_event(&mut watch).await?)?;

    let current = api
        .get_task(watched.name())
        .ok_or_else(|| io::Error::other("watched task disappeared"))?;
    api.apply_embedded_task(
        TaskManifest::from(current).with_labels(labels("development", Some("2")))?,
        watched_runtime,
    )
    .await?;
    show_event("Deleted", next_event(&mut watch).await?)?;

    api.shutdown().await?;
    Ok(())
}
