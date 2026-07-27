# solti-core

`solti-core` is the supervisor layer of the Solti SDK.

It connects three parts:

- `solti-model` - public task specs, phases, policies, and query types.
- `solti-runner` - builds runnable tasks from `TaskWorkload`.
- `taskvisor` - runs tasks, restarts them, and emits lifecycle events.

Use this crate when you want to submit tasks, query their state, cancel them,
and read their run history from one Rust API.

## Quick Start

```rust,no_run
use solti_core::{CoreError, SupervisorApi};
use solti_model::{EmbeddedSpec, RestartPolicy, TaskManifest, TaskSpec, TaskWorkload};
use solti_runner::RunnerRouter;
use taskvisor::{TaskContext, TaskError, TaskFn};

async fn demo() -> Result<(), CoreError> {
    let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;

    let task_ref = TaskFn::arc("embedded-demo", |_ctx: TaskContext| async move {
        Ok::<(), TaskError>(())
    });
    let workload = TaskWorkload::Embedded(EmbeddedSpec::new("v1")?);
    let spec = TaskSpec::builder("embedded", workload, 1_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("embedded-demo", spec)?;
    let name = manifest.name().clone();

    api.create_embedded_task(manifest, task_ref).await?;
    let _task = api.get_task(&name);
    let _runs = api.list_task_runs(&name);

    api.shutdown().await?;
    Ok(())
}
```

This example uses `create_embedded_task()` because embedded tasks are already
built as `taskvisor::TaskRef`.

Use `create_task()` when the task is described by a manifest and your
`RunnerRouter` has a runner for that workload GVK.

```rust,ignore
let mut router = RunnerRouter::new();
router.register(my_runner)?;

let api = SupervisorApi::builder(router).start().await?;
let task = api.create_task(manifest).await?;
```

## Core Model

```text
TaskSpec
  -> RunnerRouter builds a task
  -> taskvisor runs the task
  -> RuntimeObserver updates TaskState
  -> private OutputHub announces live-tail events
```

`SupervisorApi` is the main public entry point. It owns the taskvisor runtime,
the runner router, shared in-memory state, and the private output hub.

## Reconciliation Path

```text
create_task(manifest)
  -> commit desired Task state
  -> schedule reconciliation
      -> RunnerRouter builds a TaskRef
      -> map Solti policies to taskvisor policies
      -> prepare the Taskvisor submission
      -> bind it to the exact resource UID and generation
      -> submit it to Taskvisor
```

`create_embedded_task()` skips the router. It is the API for embedded Rust
tasks that already have a `taskvisor::TaskRef`.

A successful create or apply confirms the desired-state commit. Runtime
realization is asynchronous and is reported through the `Reconciled` condition.
Reconciliation is latest-wins. A stale generation cannot bind or replace the
current runtime.

## State Path

`solti-core` rebuilds task state from two paths:

- Event path: taskvisor lifecycle events update phases, attempts, run history,
  and live-tail announcements.
- Direct completion path: taskvisor `TaskWaiter` supplies the authoritative
  outcome independently of the best-effort event bus.

Final phases are selected from `TaskOutcomeKind` and `RejectionKind`. Event
`reason` text is kept only as diagnostic detail and is never parsed as schema.

For registered tasks, `TaskRemoved` is used as a FIFO barrier so attempt events
normally reach the runtime observer before identity and output cleanup. If that
best-effort barrier is lost or delayed, the direct outcome finalizes after a
bounded wait; attempt detail remains best-effort. The joined outcome reconciles
the resource-level final disposition while attempt history keeps its own result.
A concrete attempt `Timeout` stays more specific than the final
`TaskOutcomeKind::Failed`, which maps to the resource-level `Exhausted` phase.
Completion waiters run on the supervisor's construction runtime and are drained
by `shutdown()`.

## TaskState

`TaskState` is an in-memory read handle. It stores current tasks and recent
`TaskRun` records.

Common reads:

- `get_task(id)` - one task by id.
- `query_tasks(query)` - combined filters and snapshot-consistent continuation pagination.
- `list_task_runs(id)` - run history for one task.
- `state().count_by_phase()` - cheap counts for metrics.

`TaskState` clones are cheap. They share one internal `Arc<RwLock<_>>`.

The first page captures a collection `resourceVersion`. Each continuation reads
that same snapshot from retained change history. If the snapshot is no longer
retained, the query returns `CollectionError::ResourceVersionExpired`.

## Live Output

`solti-core` owns the concrete output hub and the complete task-channel
lifecycle. Runners receive only the `solti-runner::OutputPublisher` producer
capability; consumers call `SupervisorApi::subscribe_output()` and receive an
`OutputSubscription`.

The stream is lossy and live-only. A slow consumer receives
`OutputEvent::Lagged` and continues with newer events. Terminal cleanup blocks
new subscriptions; an existing subscription closes after outstanding runner
sink clones are dropped.

The builder uses `OutputConfig::default()` with a per-task capacity of 256
events. Use `with_output_config()` to choose another capacity:

```rust,no_run
use std::num::NonZeroUsize;
use solti_core::{CoreError, OutputConfig, SupervisorApi};
use solti_runner::RunnerRouter;

async fn configured_output() -> Result<(), CoreError> {
    let output = OutputConfig::new(NonZeroUsize::new(1024).unwrap());
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_output_config(output)
        .start()
        .await?;
    api.shutdown().await
}
```

## Retention

`StateConfig` controls memory retention:

| Field | Default | Meaning |
|------|---------|---------|
| `run_ttl` | 1 hour | How long finished runs stay in memory. |
| `task_ttl` | 1 hour | How long terminal tasks stay after their runs are gone. |
| `sweep_interval` | 5 minutes | How often the internal retention worker runs. |
| `max_runs_per_task` | 256 | Hard cap for retained run records per task. |
| `watch_history_capacity` | 4096 changes | History shared by watches and list continuations. |

The retention worker starts with the supervisor and stops during shutdown.

Example:

```rust
use solti_core::StateConfig;
use std::time::Duration;

let config = StateConfig::new()
    .with_run_ttl(Duration::from_secs(10 * 60))
    .with_task_ttl(Duration::from_secs(30 * 60));
```

## Error Model

Writes and runtime operations return `CoreError`.

| Variant | Meaning |
|---------|---------|
| `Supervisor` | taskvisor submit, cancel, or shutdown failed. |
| `AlreadyExists` | a live submission still owns the same task id, including a bound task between attempts. |
| `NotFound` | the requested task does not exist. |
| `Mapping` | a Solti policy could not be mapped to taskvisor. |
| `Runner` | the runner router could not build a task. |
| `InvalidSpec` | the submitted `TaskSpec` failed validation. |

`CoreError` is `#[non_exhaustive]`. Downstream code should keep a wildcard
match arm.

List and watch operations return `CollectionError`. Invalid cursors and
resource versions are separate from expired snapshots.

## Notes

- `SupervisorApiBuilder::start()` installs the runtime observer, creates the
  private output hub, and starts retention.
- `delete_task()` is idempotent and removes both task state and run history.
- `cancel_task()` returns `NotFound` when no task exists.
- Cancellation uses taskvisor's unified identity path: registered work is joined,
  while controller-queued work is removed before it starts.
