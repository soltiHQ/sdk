# solti-core
Orchestration layer for the solti task system.

Bridges `solti-model` (public API types) with the `taskvisor` runtime.
Provides `SupervisorApi` - the main entry point for submitting, querying, and cancelling tasks.

## Architecture
```text
 SupervisorApi
 ┌──────────────────────────────────────────────────────────────┐
 │                                                              │
 │  submit(spec)                                                │
 │      ├──► spec.validate()                                    │
 │      ├──► RunnerRouter::build(spec) → TaskRef                │
 │      └──► submit_with_task(task, spec)                       │
 │              ├──► state.add_task(id, spec)                   │
 │              ├──► map policies → ControllerSpec              │
 │              └──► handle.submit(controller_spec)             │
 │                                                              │
 │  taskvisor events ──► StateSubscriber ──► TaskState          │
 │                                                              │
 │  query_tasks(q) ──► TaskState ──► TaskPage<Task>             │
 │  get_task(id)   ──► TaskState ──► Option<Task>               │
 │  list_task_runs ──► TaskState ──► Vec<TaskRun>               │
 │                                                              │
 │  new(..., state_cfg) ──► auto-starts sweep task              │
 │      └──► submit_with_task(state_sweep(state, state_cfg))    │
 └──────────────────────────────────────────────────────────────┘
```

## Event flow
```text
 taskvisor runtime
     │
     ├──► TaskAdded      → (traced only; task is already in state from submit)
     ├──► TaskStarting   → transition_starting: increment_attempt + phase=Running + start_run
     ├──► TaskStopped    → transition_finished: phase=Succeeded + finish_run
     ├──► TaskFailed     → transition_finished: phase=Failed + finish_run
     ├──► TimeoutHit     → transition_finished: phase=Timeout + finish_run
     ├──► ActorExhausted → transition_finished: phase=Exhausted + finish_run
     └──► TaskRemoved    → unregister_task (tombstone: runs preserved for sweep)
```

## Key types

| Type               | Visibility | Description                                              |
|--------------------|------------|----------------------------------------------------------|
| `SupervisorApi`    | pub        | High-level facade: submit, query, cancel, sweep          |
| `StateConfig`      | pub        | TTL settings for runs, tasks, and sweep interval         |
| `CoreError`        | pub        | Error enum: Supervisor, Mapping, Runner, InvalidSpec     |
| `uptime_seconds()` | pub        | Agent uptime helper (`OnceLock<Instant>`)                |
| `TaskState`        | internal   | In-memory storage (`Arc<RwLock>`); wired by `SupervisorApi::new` |
| `StateSubscriber`  | internal   | `Subscribe` impl; auto-registered by `SupervisorApi::new` |
| `state_sweep()`    | internal   | Embedded periodic sweeper task; auto-submitted by `SupervisorApi::new` |

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

Model enums are `#[non_exhaustive]` - unknown variants fall back to safe defaults
(`DropIfRunning`, `Never`, `Full`).

## Error model
```text
 Variant       Source                       When
 ───────       ──────                       ────
 Supervisor    taskvisor runtime            submit/cancel failure
 Mapping       policy conversion            unknown policy variant
 Runner        solti_runner::RunnerError    build_task failure
 InvalidSpec   solti_model::ModelError      spec validation failure
```

## Notes
- `SupervisorApi::new` auto-registers `StateSubscriber` into the subscriber list.
- `TaskState` is `Clone` via `Arc` — safe to share across threads.
- `parking_lot::RwLock` is used instead of `std::sync::RwLock` (no poisoning, better perf).
- `unregister_task` (event-driven on `TaskRemoved`) drops the task entry but keeps runs around until sweep runs; `delete_task` (API-driven) drops both task and runs immediately.
- `uptime_seconds()` tracks agent lifetime via `OnceLock<Instant>`; initialized by `SupervisorApi::new`.
- The sweep task is self-hosted: it runs as an embedded `TaskKind::Embedded` task inside the same supervisor it manages.