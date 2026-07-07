# solti-core
Orchestration layer for the solti task system.

Bridges `solti-model` (public API types) with the `taskvisor` runtime.
Provides `SupervisorApi` - the main entry point for submitting, querying, and cancelling tasks.

## Quick start
```rust,no_run
use solti_core::{CoreError, StateConfig, SupervisorApi};
use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec};
use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, SupervisorConfig};

async fn demo() -> Result<(), CoreError> {
    let api = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        Vec::new(),          // extra event subscribers
        RunnerRouter::new(), // register runners for Subprocess/Wasm/Container here
        StateConfig::default(),
    )
    .await?;

    let kind = TaskKind::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello".into()],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("demo-slot", kind, 5_000_u64).build()?;

    let task_id = api.submit(&spec).await?;
    let _task = api.get_task(&task_id);
    let _runs = api.list_task_runs(&task_id);

    api.shutdown().await?;
    Ok(())
}
```

## Architecture
```text
 SupervisorApi
   submit(spec)
       ├──► spec.validate()                                    
       ├──► RunnerRouter::build(spec) → TaskRef
       └──► submit_with_task(task, spec)
               ├──► state.reserve(id, spec)            (atomic provisional entry)
               ├──► map policies → ControllerSpec
               └──► handle.submit_and_watch → (tv_id, TaskWaiter)
                       ├──► state.bind_tv(id, tv_id)
                       └──► spawn backstop: TaskWaiter → finalize_from_outcome
 
   taskvisor events ──► StateSubscriber ──► TaskState                          
                                        └─► OutputRegistry
                                            (RunStarted / RunFinished / evict)
 
   query_tasks(q)    ──► TaskState ──► TaskPage<Task>
   get_task(id)      ──► TaskState ──► Option<Task>
   list_task_runs    ──► TaskState ──► Vec<TaskRun>
   output_registry() ──► Arc<OutputRegistry>  (live-tail subs) 
 
   new(..., state_cfg) ──► auto-starts sweep task              
       └──► submit_with_task(state_sweep(state, state_cfg))
```

## Event flow
```text
 taskvisor runtime
     │
     ├──► TaskAdded      → (traced only; task is already in state from submit)
     ├──► TaskStarting   → transition_starting + announce_run_started
     ├──► TaskStopped    → transition_finished(Succeeded)  + announce_run_finished
     ├──► TaskFailed     → transition_finished(Failed)     + announce_run_finished
     ├──► TimeoutHit     → transition_finished(Timeout)    + announce_run_finished
     ├──► ActorExhausted → transition_finished(Exhausted)  + announce_run_finished + evict
     ├──► ActorDead      → transition_finished(Failed)     + announce_run_finished + evict
     └──► TaskRemoved    → unregister_task                 + evict
```

`announce_*` and `evict` reach an `Arc<OutputRegistry>` shared with the runner side; 
this is what bridges supervisor lifecycle into the live-tail broadcast channel that subscribers (HTTP SSE, gRPC stream) read from.

## Key types

| Type               | Visibility | Description                                                                                         |
|--------------------|------------|-----------------------------------------------------------------------------------------------------|
| `SupervisorApi`    | pub        | High-level facade: submit, query, cancel, sweep, `output_registry()` accessor                       |
| `StateConfig`      | pub        | TTL settings for runs, tasks, and sweep interval                                                    |
| `CoreError`        | pub        | Error enum (`#[non_exhaustive]`): Supervisor, AlreadyExists, NotFound, Mapping, Runner, InvalidSpec |
| `uptime_seconds()` | pub        | Agent uptime helper (`OnceLock<Instant>`)                                                           |
| `TaskState`        | internal   | In-memory storage (`Arc<RwLock>`); wired by `SupervisorApi::new`                                    |
| `StateSubscriber`  | internal   | `Subscribe` impl; auto-registered by `SupervisorApi::new`                                           |
| `state_sweep()`    | internal   | Embedded periodic sweeper task; auto-submitted by `SupervisorApi::new`                              |

## State storage
```text
 TaskState (Arc<RwLock<TaskStateInner>>)
 ┌──────────────────────────────────────────────┐
 │  tasks:   HashMap<TaskId, Task>              │
 │  by_slot: HashMap<Slot, Vec<TaskId>>         │ ← index for slot queries
 │  runs:    HashMap<TaskId, VecDeque<TaskRun>> │
 └──────────────────────────────────────────────┘
```

Queries use the `by_slot` index when a slot filter is present to avoid full scans.
Pagination is deterministic (sorted by `TaskId`).

## State sweep
```text
 SupervisorApi::new(..., StateConfig)
     └──► auto-starts embedded periodic task (slot: "solti-state-sweep")
           ├──► pass 1: remove finished runs older than run_ttl
           └──► pass 2: remove terminal tasks with no runs past task_ttl
```

| Parameter        | Default   | Controls                                        |
|------------------|-----------|-------------------------------------------------|
| `run_ttl`        | 1 hour    | How long finished runs are retained             |
| `task_ttl`       | 1 hour    | How long terminal tasks are retained            |
| `sweep_interval` | 5 minutes | Sweep frequency (via `RestartPolicy::periodic`) |

In addition to the TTLs, `StateConfig::max_runs_per_task` (default `256`) caps the retained run history per task: 
when a new attempt starts, the oldest *finished* runs beyond the cap are evicted (the in-flight run is never dropped). 
This bounds memory for fast-restarting tasks *between* sweeps.

Sweep is always-on. Configure TTLs via `StateConfig` if defaults don't fit.

## Policy mapping
```text
 solti-model                    taskvisor
 ───────────                    ────────
 AdmissionPolicy::Replace   →  AdmissionPolicy::Replace
 RestartPolicy::OnFailure   →  RestartPolicy::OnFailure
 JitterPolicy::Equal        →  JitterPolicy::Equal
 BackoffPolicy { first_ms } →  BackoffPolicy { first: Duration }
```

The model enums are `#[non_exhaustive]`. The mappers therefore carry a wildcard arm: 
an unknown variant produces a `CoreError::Mapping` (surfaced as `500` by solti-api), never a silent fallback to a default policy.

## Error model
```text
 Variant       Source                       When                              HTTP
 ───────       ──────                       ────                              ────
 Supervisor    taskvisor runtime            submit/cancel/remove failure      500
 AlreadyExists name already active          duplicate non-terminal submit     409
 NotFound      no such task                 cancel/delete of a missing task   404
 Mapping       policy conversion            unknown model policy variant      500
 Runner        solti_runner::RunnerError    build_task failure                500
 InvalidSpec   solti_model::ModelError      spec validation failure           400
```

`CoreError` is `#[non_exhaustive]`; solti-api maps every variant above and falls through to `500` for any future one.

## Notes
- `SupervisorApi::new` auto-registers `StateSubscriber` into the subscriber list.
- `SupervisorApi::new` creates a fresh empty `OutputRegistry`; use `new_with_output_registry(...)` to share one with the runner side.
- `TaskState` is `Clone` via `Arc` — safe to share across threads.
- `parking_lot::RwLock` is used instead of `std::sync::RwLock` (no poisoning, better perf).
- `unregister_task` (event-driven on `TaskRemoved`) drops the task entry but keeps runs around until sweep runs; `delete_task` (API-driven) drops both task and runs immediately.
- `uptime_seconds()` tracks agent lifetime via `OnceLock<Instant>`; initialized by `SupervisorApi::new`.
- The sweep task is self-hosted: it runs as an embedded `TaskKind::Embedded` task inside the same supervisor it manages.