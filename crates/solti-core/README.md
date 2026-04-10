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
 │  enable_gc(config)                                           │
 │      └──► submit_with_task(state_gc(state, config))          │
 └──────────────────────────────────────────────────────────────┘
```

## Event flow
```text
 taskvisor runtime
     │
     ├──► TaskAdded      → (logged, task already in state via submit)
     ├──► TaskStarting   → increment_attempt + phase=Running + start_run
     ├──► TaskStopped    → phase=Succeeded + finish_run
     ├──► TaskFailed     → phase=Failed + finish_run
     ├──► TimeoutHit     → phase=Timeout + finish_run
     ├──► ActorExhausted → phase=Exhausted + finish_run
     └──► TaskRemoved    → remove_task (runs preserved for GC)
```

## Key types

| Type               | Description                                              |
|--------------------|----------------------------------------------------------|
| `SupervisorApi`    | High-level facade: submit, query, cancel, GC             |
| `TaskState`        | In-memory storage: tasks + runs (`Arc<RwLock>`)          |
| `StateSubscriber`  | `Subscribe` impl wiring events into `TaskState`          |
| `StateConfig`      | TTL settings for runs, tasks, and GC interval            |
| `CoreError`        | Error enum: Supervisor, Mapping, Runner, InvalidSpec     |

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

## Garbage collection
```text
 enable_gc(StateConfig)
     └──► embedded periodic task (slot: "solti-state-gc")
           ├──► sweep pass 1: remove finished runs older than run_ttl
           └──► sweep pass 2: remove terminal tasks with no runs past task_ttl
```

| Parameter     | Default   | Controls                                        |
|---------------|-----------|-------------------------------------------------|
| `run_ttl`     | 1 hour    | How long finished runs are retained             |
| `task_ttl`    | 1 hour    | How long terminal tasks are retained            |
| `gc_interval` | 5 minutes | Sweep frequency (via `RestartPolicy::periodic`) |

GC is opt-in. If not enabled, state grows unboundedly.

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
- `TaskState` is `Clone` via `Arc` - safe to share across threads.
- `parking_lot::RwLock` is used instead of `std::sync::RwLock` (no poisoning, better perf).
- `remove_task` (event-driven) preserves runs; `delete_task` (API-driven) removes both.
- `uptime_seconds()` tracks agent lifetime via `OnceLock<Instant>`.
- The GC task is self-hosted: it runs as an embedded `TaskKind::Embedded` task inside the same supervisor it manages.
