# solti-core

`solti-core` is the supervisor layer of the Solti SDK.

It connects three parts:

- `solti-model` - public task specs, phases, policies, and query types.
- `solti-runner` - builds runnable tasks from `TaskKind`.
- `taskvisor` - runs tasks, restarts them, and emits lifecycle events.

Use this crate when you want to submit tasks, query their state, cancel them,
and read their run history from one Rust API.

## Quick Start

```rust,no_run
use solti_core::taskvisor::{
    ControllerConfig, SupervisorConfig, TaskContext, TaskError, TaskFn,
};
use solti_core::{CoreError, RunnerRouter, StateConfig, SupervisorApi};
use solti_model::{RestartPolicy, TaskKind, TaskSpec};

async fn demo() -> Result<(), CoreError> {
    let api = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        Vec::new(),
        RunnerRouter::new(),
        StateConfig::default(),
    )
    .await?;

    let task = TaskFn::arc("embedded-demo", |_ctx: TaskContext| async move {
        Ok::<(), TaskError>(())
    });
    let spec = TaskSpec::builder("embedded", TaskKind::Embedded, 1_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;

    let task_id = api.submit_with_task(task, &spec).await?;
    let _task = api.get_task(&task_id);
    let _runs = api.list_task_runs(&task_id);

    api.shutdown().await?;
    Ok(())
}
```

This example uses `submit_with_task()` because embedded tasks are already built
as `taskvisor::TaskRef`.

Use `submit()` when the task is described only by `TaskSpec` and your
`RunnerRouter` has a runner for that `TaskKind`.

```rust,ignore
let mut router = RunnerRouter::new();
router.register(my_runner);

let api = SupervisorApi::new(
    SupervisorConfig::default(),
    ControllerConfig::default(),
    Vec::new(),
    router,
    StateConfig::default(),
)
.await?;

let task_id = api.submit(&spec).await?;
```

## Core Model

```text
TaskSpec
  -> RunnerRouter builds a task
  -> taskvisor runs the task
  -> StateSubscriber updates TaskState
  -> OutputRegistry announces live-tail events
```

`SupervisorApi` is the main public entry point. It owns the taskvisor runtime,
the runner router, shared in-memory state, and the output registry.

## Submit Path

```text
submit(spec)
  -> spec.validate()
  -> RunnerRouter::build(spec)
  -> submit_with_task(task, spec)
      -> reserve a TaskState entry
      -> map Solti policies to taskvisor policies
      -> prepare the taskvisor submission identity
      -> bind the Solti TaskId to the taskvisor submission identity
      -> submit to taskvisor's bounded controller command queue
```

`submit_with_task()` skips the router. It is the right API for embedded Rust
tasks that already have a `taskvisor::TaskRef`.

The SDK installs the local identity binding before controller intake, while no
event for that prepared identity can exist yet. `Ok(TaskId)` confirms queue
intake. Slot admission, runtime registration, and task-body start still happen
asynchronously. A later admission rejection is delivered through the direct
completion waiter and becomes a terminal task state. If intake fails, or the
submit future is cancelled before intake, the provisional state reservation is
rolled back; a previously retained terminal resource with the same id is restored.

## State Path

`solti-core` rebuilds task state from two paths:

- Event path: taskvisor lifecycle events update phases, attempts, run history,
  and live-tail announcements.
- Direct completion path: taskvisor `TaskWaiter` supplies the authoritative
  outcome independently of the best-effort event bus.

Final phases are selected from `TaskOutcomeKind` and `RejectionKind`. Event
`reason` text is kept only as diagnostic detail and is never parsed as schema.

For registered tasks, `TaskRemoved` is used as a FIFO barrier so attempt events
normally reach the state subscriber before identity and output cleanup. If that
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
- `list_all_tasks()` - public tasks, excluding internal Solti tasks.
- `query_tasks(query)` - combined filters and pagination.
- `list_task_runs(id)` - run history for one task.
- `state().count_by_phase()` - cheap counts for metrics.

`TaskState` clones are cheap. They share one internal `Arc<RwLock<_>>`.

## Output Registry

`SupervisorApi::output_registry()` returns the shared `OutputRegistry`.

Runners write output chunks into it. API layers subscribe to it for live logs,
for example HTTP SSE and gRPC server streams.

## Retention

`StateConfig` controls memory retention:

| Field | Default | Meaning |
|------|---------|---------|
| `run_ttl` | 1 hour | How long finished runs stay in memory. |
| `task_ttl` | 1 hour | How long terminal tasks stay after their runs are gone. |
| `sweep_interval` | 5 minutes | How often the embedded sweep task runs. |
| `max_runs_per_task` | 256 | Hard cap for retained run records per task. |

The sweep task starts automatically in `SupervisorApi::new()`.

Example:

```rust
use solti_core::StateConfig;
use std::time::Duration;

let config = StateConfig {
    run_ttl: Duration::from_secs(10 * 60),
    task_ttl: Duration::from_secs(30 * 60),
    ..StateConfig::default()
};
```

## Error Model

All fallible APIs return `CoreError`.

| Variant | Meaning |
|---------|---------|
| `Supervisor` | taskvisor submit, cancel, or shutdown failed. |
| `AlreadyExists` | a live submission still owns the same task id, including a bound task between attempts. |
| `NotFound` | the requested task does not exist. |
| `Mapping` | a Solti policy could not be mapped to taskvisor. |
| `Runner` | the runner router could not build a task. |
| `InvalidSpec` | the submitted `TaskSpec` failed validation. |

`CoreError` is `#[non_exhaustive]`, so downstream code should keep a wildcard
match arm.

## Re-exports

`solti-core` re-exports:

- `taskvisor` - so applications use the same runtime version.
- `RunnerRouter` and `OutputRegistry` - the runner-facing pieces used with the
  supervisor.

## Notes

- `SupervisorApi::new()` registers the state subscriber automatically and uses
  the router's output registry for API live-tail streams.
- `new_with_output_registry()` replaces the router registry with the supplied
  one, preserving the same output-path invariant.
- `delete_task()` is idempotent and removes both task state and run history.
- `cancel_task()` returns `NotFound` when no task exists.
- Cancellation uses taskvisor's unified identity path: registered work is joined,
  while controller-queued work is removed before it starts.
- `uptime_seconds()` reports process uptime from the first `SupervisorApi::new()`.
