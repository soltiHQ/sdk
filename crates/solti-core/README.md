# solti-core

`solti-core` is the desired-state supervisor of the Solti SDK.
It connects `solti-model`, `solti-runner`, and Taskvisor.
It stores task resources in memory.
It reconciles workloads and tracks execution state.
It also exposes live output.

Use this crate when an application needs one Rust API for task submission and state.
The same API provides history, output, cancellation, and shutdown.
It does not expose a network API or persist resources.

## Quick start

Create an embedded task when the application already owns its `taskvisor::TaskRef`:

```rust,no_run
use solti_core::{CoreError, SupervisorApi};
use solti_model::{EmbeddedSpec, TaskManifest, TaskSpec, TaskWorkload};
use solti_runner::RunnerRouter;
use taskvisor::{TaskContext, TaskError, TaskFn};

async fn run() -> Result<(), CoreError> {
    let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;

    let task_ref = TaskFn::arc("cleanup-runtime", |_ctx: TaskContext| async move {
        Ok::<(), TaskError>(())
    });
    let workload = TaskWorkload::Embedded(EmbeddedSpec::new("cleanup-v1")?);
    let spec = TaskSpec::builder("maintenance", workload, 5_000_u64).build()?;
    let manifest = TaskManifest::new("cleanup", spec)?;
    let name = manifest.name().clone();

    let committed = api.create_embedded_task(manifest, task_ref).await?;
    assert_eq!(committed.name(), &name);
    assert!(api.get_task(&name).is_some());

    api.shutdown().await
}
```

A successful create commits desired state.
Runtime realization continues asynchronously.
Read the `Reconciled` condition to observe its result.

## What it does

- commits Kubernetes-style `Task` desired state before runtime work starts;
- routes workloads through `RunnerRouter` or accepts an embedded `TaskRef`;
- reports runtime intake through the `Reconciled` condition;
- stores current tasks and recent `TaskRun` history in memory;
- provides filtered reads, snapshot pagination, and resumable watches;
- exposes bounded live output for each task;
- cancels, deletes, and retains task resources;
- owns Taskvisor startup and shutdown.

## Inputs and outputs

| API                                      | Input                                      | Output                                           |
|------------------------------------------|--------------------------------------------|--------------------------------------------------|
| `SupervisorApi::builder`                 | `RunnerRouter`                             | Configurable supervisor builder                  |
| `create_task` / `apply_task`             | Routed `TaskManifest`                      | Committed `Task`                                 |
| `create_embedded_task`                   | Embedded `TaskManifest` and `TaskRef`      | Committed `Task`                                 |
| `get_task`                               | `TaskId`                                   | Current retained `Task`                          |
| `query_tasks`                            | `TaskQuery`                                | Snapshot-consistent `TaskPage<Task>`             |
| `watch_tasks`                            | `TaskFilter` and optional resource version | `TaskWatchSubscription`                          |
| `list_task_runs`                         | `TaskId`                                   | Oldest-first `Vec<TaskRun>`                      |
| `subscribe_output`                       | `TaskId`                                   | Optional live `OutputSubscription`               |
| `cancel_task`                            | `TaskId`                                   | Runtime cancellation while desired state remains |
| `delete_task`                            | `TaskId`                                   | Runtime stop and resource removal                |
| `shutdown`                               | Running supervisor                         | Drained SDK-owned runtime                        |

## Submission paths

| Workload                       | API                                                        |
|--------------------------------|------------------------------------------------------------|
| Routed by workload GVK         | `create_task` or `apply_task`                              |
| Prebuilt in-process `TaskRef`  | `create_embedded_task` or `apply_embedded_task`            |
| Conditional routed apply       | `apply_task_with_preconditions`                            |
| Conditional embedded apply     | `apply_embedded_task_with_preconditions`                   |
| Adapter-owned visibility check | `apply_task_where`                                         |

Routed submission requires a non-embedded workload.
The router selects a registered runner by workload GVK and optional labels.
Runner construction happens in the reconciliation worker.

Embedded submission requires `TaskWorkload::Embedded`.
The caller supplies the matching `TaskRef`.
Embedded tasks bypass `RunnerRouter`.

Passing a routed workload to an embedded method is rejected.
Passing an embedded workload to a routed method is also rejected.

## Create and apply

`create_*` rejects every retained resource with the same `metadata.name`.
This includes terminal resources waiting for retention.

`apply_*` creates a missing resource when no preconditions are supplied.
It updates labels, annotations, and desired spec on an existing resource.
Metadata-only changes keep the generation.
Spec changes advance the generation and reset execution status to pending.

An identical apply is a true no-op after successful reconciliation.
An identical apply schedules one manual retry when `Reconciled=False`.

Use `WritePreconditions` to protect a write from stale state:

```rust,no_run
use solti_core::{CoreError, SupervisorApi};
use solti_model::{Task, TaskManifest, WritePreconditions};

async fn guarded_apply(
    api: &SupervisorApi,
    manifest: TaskManifest,
) -> Result<Task, CoreError> {
    let current = api
        .get_task(manifest.name())
        .ok_or_else(|| CoreError::NotFound(manifest.name().to_string()))?;
    let preconditions = WritePreconditions::from_task(&current)?;

    api.apply_task_with_preconditions(manifest, preconditions)
        .await
}
```

Preconditions can check UID, resource version, or both.
A conditional apply does not create a missing resource.
Failed checks return `CoreError::Conflict`.

## Observe reconciliation

Create and apply return after desired state is committed.
They do not wait for runtime acceptance or task completion.

The required `Reconciled` condition reports the reconciliation result:

| Status    | Meaning                                                       |
|-----------|---------------------------------------------------------------|
| `Unknown` | The desired generation is waiting for reconciliation          |
| `True`    | The desired generation was accepted by the runtime            |
| `False`   | Routing, construction, mapping, or runtime intake failed       |

`reason`, `message`, and `observedGeneration` carry reconciliation diagnostics.
Reconciliation failures do not use the execution `phase`, `error`, or `exitCode` fields.

Reconciliation uses latest-wins semantics.
A stale generation cannot bind or replace the current runtime.
Side effects accepted before a generation becomes stale are not rolled back.
The crate does not provide staged rollout or availability guarantees.

## Read tasks

Use `get_task()` for one retained resource.
Use `query_tasks()` for combined filtering and pagination:

```rust
use solti_core::{CollectionError, SupervisorApi};
use solti_model::{TaskPhase, TaskQuery};

fn read_running(api: &SupervisorApi) -> Result<(), CollectionError> {
    let query = TaskQuery::new()
        .with_phase(TaskPhase::Running)
        .with_limit(100);
    let first = api.query_tasks(&query)?;

    if let Some(continuation) = first.continuation.clone() {
        let next = api.query_tasks(&query.with_continuation(continuation))?;
        assert_eq!(next.resource_version, first.resource_version);
    }

    Ok(())
}
```

The first page captures one collection resource version.
Every continuation reads the same retained snapshot.
Items are ordered by task name.
Filtering happens before pagination.

## Watch tasks

Use `watch_tasks()` to follow a collection:

```rust,no_run
use solti_core::{CollectionError, SupervisorApi};
use solti_model::{TaskFilter, TaskQuery};
use tokio_stream::StreamExt;

async fn watch_tasks(
    api: &SupervisorApi,
) -> Result<(), CollectionError> {
    let page = api.query_tasks(&TaskQuery::new())?;
    let mut watch = api.watch_tasks(
        &TaskFilter::new(),
        Some(&page.resource_version),
    )?;

    while let Some(event) = watch.next().await {
        println!("{:?}", event?);
    }

    Ok(())
}
```

An absent resource version or `"0"` emits the current sorted snapshot first.
An exact retained resource version replays later changes before live changes.
The watch ends during supervisor shutdown.

List continuations and watches share retained change history.
Compacted or foreign versions return `CollectionError::ResourceVersionExpired`.
Malformed or future versions return `CollectionError::InvalidResourceVersion`.

`query_tasks_where()` and `watch_tasks_where()` add an adapter-owned visibility predicate.
Core itself does not hide embedded workloads.
A network adapter can filter them before pagination and watch transition classification.

## Run history

`list_task_runs()` returns retained runs ordered by generation and attempt.
Each run snapshots the workload GVK of its generation.
An unknown task or swept history returns an empty list.

Attempt events provide run detail.
The direct Taskvisor outcome supplies the authoritative resource-level completion.
This second path can finalize a task when a best-effort event was dropped.

Typed `TaskOutcomeKind` and `RejectionKind` values select terminal phases.
Diagnostic event text never selects a phase.

## Live output

Runners publish output through the `OutputPublisher` installed by core.
Consumers subscribe through `SupervisorApi::subscribe_output()`:

```rust,no_run
use solti_core::SupervisorApi;
use solti_model::TaskId;
use solti_model::OutputEvent;
use tokio_stream::StreamExt;

async fn read_output(api: &SupervisorApi, name: &TaskId) {
    let Some(mut output) = api.subscribe_output(name) else {
        return;
    };

    while let Some(event) = output.next().await {
        match event {
            OutputEvent::Chunk(chunk) => {
                println!("{:?}: {:?}", chunk.stream, chunk.line);
            }
            OutputEvent::Lagged { skipped } => {
                eprintln!("skipped {skipped} output events");
            }
            OutputEvent::RunStarted { .. } | OutputEvent::RunFinished { .. } => {}
            _ => {}
        }
    }
}
```

Output is live-only.
New subscribers do not receive historical chunks.
Each task has one bounded broadcast ring shared across attempts.
A slow subscriber receives `OutputEvent::Lagged` and then continues.

Terminal cleanup blocks new subscriptions.
Existing subscriptions close after every outstanding runner sink clone is dropped.
A stale sink remains attached to the old channel when a task name is reused.

## Cancel and delete

`cancel_task()` stops the current bound or queued runtime.
It keeps the desired resource and run history.
Cancellation before a runtime binding exists is a no-op for a known resource.
An unknown name returns `CoreError::NotFound`.

`delete_task()` stops the runtime and removes the resource and its runs.
Deleting a missing resource is an idempotent no-op.

`delete_task_with_preconditions()` rejects a missing resource and stale guards.
`delete_task_where()` also applies an adapter-owned visibility predicate under the same per-name operation lock.

## Configuration

`StateConfig` controls in-memory retention:

| Field                       | Default      | Meaning                                                    |
|-----------------------------|--------------|------------------------------------------------------------|
| `run_ttl`                   | 1 hour       | Age limit for finished runs and unbound orphaned runs      |
| `task_ttl`                  | 1 hour       | Age limit for terminal tasks after run history is empty    |
| `sweep_interval`            | 5 minutes    | Retention worker interval                                  |
| `max_runs_per_task`         | 256          | Per-task completed run cap                                 |
| `watch_history_capacity`    | 4096 changes | Maximum retained collection changes                        |
| `watch_history_byte_budget` | 64 MiB       | Maximum serialized task bytes in collection change history |

`max_runs_per_task = 0` removes completed history while keeping active runs.
`sweep_interval = 0` is rejected.
Watch history capacity and byte budget must also be greater than zero.

`OutputConfig` controls the live-output ring.
Its default capacity is 256 events per task.
Zero is rejected.

```rust,no_run
use std::time::Duration;

use solti_core::{OutputConfig, StateConfig, SupervisorApi};
use solti_runner::RunnerRouter;

async fn configured() -> Result<(), Box<dyn std::error::Error>> {
    let state = StateConfig::new()
        .with_run_ttl(Duration::from_secs(10 * 60))
        .with_task_ttl(Duration::from_secs(30 * 60))
        .try_with_sweep_interval(Duration::from_secs(60))?
        .with_max_runs_per_task(64)
        .try_with_watch_history_capacity(1024)?
        .try_with_watch_history_byte_budget(16 * 1024 * 1024)?;
    let output = OutputConfig::try_new(1024)?;

    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_config(state)
        .with_output_config(output)
        .start()
        .await?;

    api.shutdown().await?;
    Ok(())
}
```

The builder also accepts Taskvisor runtime configuration, controller configuration, and external Taskvisor subscribers.
The core state observer is always installed.

## Persistence hooks

Core can forward committed task state changes and task output to optional application-owned sinks.

The hooks are storage-neutral. 
They do not add a database, replace the in-memory `TaskState`, retry delivery, or make state durable by themselves.
They are write-side notifications; core does not load persisted state during startup.

Callbacks run synchronously on core and runner paths. 
They must return quickly, must not call back into core, and should normally forward cloned events to an application-owned worker:

```rust,no_run
use std::sync::{Arc, mpsc};

use solti_core::{
    SupervisorApi, TaskOutputEvent, TaskOutputSink, TaskStateEvent, TaskStateSink,
};
use solti_runner::RunnerRouter;

struct StateForwarder(mpsc::Sender<TaskStateEvent>);

impl TaskStateSink for StateForwarder {
    fn on_event(&self, event: &TaskStateEvent) {
        let _ = self.0.send(event.clone());
    }
}

struct OutputForwarder(mpsc::Sender<TaskOutputEvent>);

impl TaskOutputSink for OutputForwarder {
    fn on_event(&self, event: &TaskOutputEvent) {
        let _ = self.0.send(event.clone());
    }
}

async fn persistent_agent() -> Result<(), Box<dyn std::error::Error>> {
    let (state_tx, _state_rx) = mpsc::channel();
    let (output_tx, _output_rx) = mpsc::channel();

    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(Arc::new(StateForwarder(state_tx)))
        .with_output_sink(Arc::new(OutputForwarder(output_tx)))
        .start()
        .await?;

    api.shutdown().await?;
    Ok(())
}
```

`TaskStateEvent` reports task create, apply, status, delete, and run start or finish changes. 
Run events carry the task name and UID. 
Run retention does not emit delete events.

`TaskOutputSink` is installed before runners start and receives the same published chunks and run markers as the live output path, including the first published event. 
Each event carries the task name and UID, so a late event from a deleted task cannot be confused with a recreated task using the same name.
Subscriber-local `Lagged` notifications are not persisted.
Sink panics are caught and logged. The sink owns database errors, retries, queue limits, and shutdown draining.

## Shutdown

Call `shutdown()` when cleanup must finish before the application continues.

Shutdown closes task watches.
It stops Taskvisor and waits for reconciliation, completion, and retention workers.

Dropping `SupervisorApi` starts the same cleanup path in the background.
It does not provide an awaitable result.

## Specific behavior

- `metadata.name` is the stable task address; Taskvisor IDs remain internal.
- Resource versions are opaque and belong to one `TaskState` incarnation.
- `TaskState` clones share the same in-memory store.
- Core does not persist tasks, runs, output, or watch history by itself.
- Optional persistence hooks can forward task, run, and output events to an application-owned store.
- Core does not hide embedded or extension workloads.
- Adapter predicates run before pagination and watch event classification.
- Retention never removes a task with a runtime binding.
- The built-in output subscription is live-only and may report `OutputEvent::Lagged`.
- A watch can resume after lag only while its resource version remains retained.
- Dropping `SupervisorApi` starts cleanup but cannot report its result.

## Errors

Public write and lifecycle methods return `CoreError`:

| Variant               | Cause                                                    |
|-----------------------|----------------------------------------------------------|
| `StateInitialization` | The state resource-version identity could not initialize |
| `ShuttingDown`        | A desired-state write started after shutdown             |
| `Supervisor`          | Taskvisor prepare, submit, cancel, or shutdown failed    |
| `AlreadyExists`       | Create found a retained resource with the same name      |
| `NotFound`            | A required resource does not exist or is hidden          |
| `Conflict`            | UID or resource-version preconditions failed             |
| `Mapping`             | A model policy has no Taskvisor mapping                  |
| `Runner`              | Runner selection or task construction failed             |
| `InvalidSpec`         | The submitted model or workload path is invalid          |

Create and apply do not return asynchronous reconciliation failures.
Runner, mapping, prepare, and submit failures set `Reconciled=False`.

`CoreError` is non-exhaustive.
Keep a wildcard arm when matching it.

Collection reads use `CollectionError`.
Checked configuration uses `ConfigError`.

## Examples

### Internal examples

These examples exercise only desired state, reconciliation, collections, and live output.
They do not expose a network API, perform discovery, or add durable storage.
Each example starts with a text flow diagram, then explains its inputs, transitions, and result.

Start with a routed workload and its live output:

```bash
cargo run -p solti-core --example routed_output
```

| Example                                                 | What it shows                                                                 |
|---------------------------------------------------------|-------------------------------------------------------------------------------|
| [routed_output.rs](examples/routed_output.rs)           | Runner reconciliation, desired-state commit, live output, and final status.   |
| [embedded_lifecycle.rs](examples/embedded_lifecycle.rs) | Embedded revisions, generation replacement, cancellation, and run history.    |
| [collections.rs](examples/collections.rs)               | Snapshot-consistent pagination and filter-relative task watch event kinds.    |

Run the remaining examples explicitly:

```bash
cargo run -p solti-core --example embedded_lifecycle
cargo run -p solti-core --example collections
```

### Full examples

Application-level compositions live in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
They combine core with concrete execution runners, discovery, observability, and the agent API.

## Contributor guide

See the [solti-core source guide](https://github.com/soltiHQ/sdk/blob/main/crates/solti-core/ARCHITECTURE.md) for module ownership, runtime flows, concurrency, and invariants.
