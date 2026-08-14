# solti-core source guide

This document is a reading map for contributors.

It shows which module owns each decision and how desired state reaches Taskvisor.
The Rust source and its module-level documentation remain the source of truth.

## Crate map

`SupervisorApi` is the public lifecycle boundary.
It coordinates state, reconciliation, output, and Taskvisor.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart TB
    Public["lib.rs<br/>public re-exports"]
    Builder["supervisor/builder.rs<br/>construction"]
    Api["supervisor/mod.rs<br/>public operations"]
    State["state/mod.rs<br/>authoritative resources"]
    Reconciler["runtime/reconciler.rs<br/>runtime intake"]
    Observer["runtime/observer.rs<br/>event and outcome projection"]
    Locks["runtime/locks.rs<br/>keyed operation locks"]
    Output["output.rs<br/>live output channels"]
    Persistence["persistence.rs<br/>external event sinks"]
    Map["map/<br/>Taskvisor mapping"]
    Config["config.rs<br/>admission and retention settings"]
    Error["error.rs<br/>public errors"]

    Public --> Builder
    Public --> Api
    Public --> State
    Public --> Output
    Public --> Persistence
    Public --> Config
    Public --> Error

    Builder --> Api
    Api --> State
    Api --> Reconciler
    Api --> Locks
    Reconciler --> State
    Reconciler --> Observer
    Reconciler --> Output
    Reconciler --> Map
    Observer --> State
    Observer --> Output
    State --> Persistence
    Output --> Persistence
```

The arrows show direct use.
They do not represent ownership of model values.

| Module                  | Owns                                                                   | Does not own                               |
|-------------------------|------------------------------------------------------------------------|--------------------------------------------|
| `supervisor/builder.rs` | Runtime assembly and public configuration                              | Desired-state writes or task execution     |
| `supervisor/mod.rs`     | Public API operations, write scheduling, cancellation, shutdown        | Runner implementations or model validation |
| `state/mod.rs`          | Tasks, runs, runtime bindings, resource versions, list and watch state | Taskvisor execution or durable persistence |
| `runtime/reconciler.rs` | Runner preflight, replacement, binding, submission, completion waiters | Public collection queries                  |
| `runtime/observer.rs`   | Taskvisor event projection and authoritative finalization              | Desired-state admission                    |
| `runtime/locks.rs`      | Weak keyed locks for one operation class                               | Resource state                             |
| `output.rs`             | Per-task live broadcast channels                                       | Output history or persistence              |
| `persistence.rs`        | Optional task-state and output event sink contracts                    | Storage, retries, or recovery              |
| `map/`                  | Typed Solti-to-Taskvisor policy and outcome mapping                    | Routing or runtime ownership               |
| `config.rs`             | State admission, retention, and watch journal settings                  | Worker scheduling                          |
| `error.rs`              | Public operation and write-conflict errors                             | Reconciliation status storage              |

## Runtime construction

`SupervisorApiBuilder::start` assembles every runtime-owned component.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart TB
    Builder["SupervisorApiBuilder"]
    Output["OutputHub"]
    Sinks["Optional persistence sinks"]
    Router["RunnerRouter<br/>with OutputPublisher"]
    State["TaskState"]
    Observer["RuntimeObserver"]
    Taskvisor["Taskvisor Supervisor"]
    Handle["SupervisorHandle"]
    Reconciler["Reconciler"]
    Retention["Retention worker"]
    Api["SupervisorApi"]

    Builder --> Output
    Builder --> Sinks
    Sinks --> Output
    Output --> Router
    Builder --> State
    Sinks --> State
    State --> Observer
    Output --> Observer
    Observer -->|installed subscriber| Taskvisor
    Builder --> Taskvisor
    Taskvisor --> Handle
    Handle --> Reconciler
    Router --> Reconciler
    State --> Reconciler
    Observer --> Reconciler
    Output --> Reconciler
    Reconciler --> Retention
    Reconciler --> Api
```

The builder always installs the core observer.
External Taskvisor subscribers are added beside it.

The router receives the core-owned `OutputPublisher`.
Runners can publish output without owning consumer subscriptions.

`SupervisorApi` owns one `Reconciler`.
The reconciler shares its dependencies with keyed reconciliation coordinators and completion workers.

## Desired-state writes

Create and apply commit a complete `Task` before runtime work starts.

```mermaid
sequenceDiagram
    participant Caller
    participant API as SupervisorApi
    participant Lock as Desired operation lock
    participant State as TaskState
    participant Worker as Reconciliation worker

    Caller->>API: create or apply TaskManifest
    API->>API: verify routed or embedded path
    API->>Lock: lock metadata.name
    API->>API: reject writes after shutdown starts
    API->>State: create or apply desired state
    State->>State: check retained count and TaskManifest bytes under one write lock
    State-->>API: committed Task + reconcile decision
    API->>Worker: schedule when required
    API-->>Caller: committed Task
```

The desired operation lock serializes writes by task name.
Different names remain independent.

Create rejects every retained resource with the same name.
Apply creates a missing resource only when preconditions are empty.

Retained Task admission and desired-state mutation use the same state write
lock. The current Task count and aggregate TaskManifest byte budget are
independent. The byte budget measures only compact canonical `TaskManifest`
JSON.

Every current Task counts, including embedded, pending, running, and terminal
Tasks.
At the configured count limit, a new name fails. The count limit does not reject
an apply to an existing Task. A new name or positive-growth existing apply fails
when it would exceed the TaskManifest byte budget. Shrinking and no-op applies
remain allowed. Admission does not evict a Task or wait for capacity.

A duplicate create returns `AlreadyExists` before admission errors. A guarded
missing apply remains `NotFound`; an unchecked missing apply uses create
admission.
A rejected admission does not advance the resource version, update indexes or
watch history, publish persistence events, or schedule reconciliation.

Apply has four outcomes:

| Change                            | Resource version | Generation | Reconciliation |
|-----------------------------------|------------------|------------|----------------|
| No change after success           | Unchanged        | Unchanged  | Not scheduled  |
| Metadata only                     | Advanced         | Unchanged  | Not scheduled  |
| Spec                              | Advanced         | Advanced   | Scheduled      |
| No change with `Reconciled=False` | Advanced         | Unchanged  | Scheduled once |

UID and resource-version preconditions are checked under the state write lock.
A failed check does not consume a resource version.

The spawn gate orders desired commits against shutdown.
No reconciliation coordinator can be registered after shutdown closes the tracker.

## Submission paths

The manifest workload and runtime source must agree.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Manifest["Committed Task"]
    Source{"Submission path"}
    Routed["Routed"]
    Embedded["Embedded"]
    Router["RunnerRouter<br/>GVK + selector"]
    TaskRef["Caller TaskRef"]
    Prepared["PreparedSubmission"]

    Manifest --> Source
    Source --> Routed
    Source --> Embedded
    Routed --> Router
    Router --> Prepared
    Embedded --> TaskRef
    TaskRef --> Prepared
```

Routed methods reject `TaskWorkload::Embedded`.
Embedded methods reject every routed workload.

The router selects a runner by workload GVK and optional labels.
The runner builds one Taskvisor `TaskRef`.

Embedded submission receives the `TaskRef` from the caller.
It bypasses runner routing.

Both paths use the same policy mapping, runtime binding, submission, observation, and cleanup.

## Reconciliation

One reconciliation targets an exact resource UID and generation.
`ResourceGeneration` also snapshots the workload GVK.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}, "themeVariables": {"fontSize": "12px"}}}%%
flowchart TB
    Desired["Committed generation"]
    Current1{"Still current?"}
    Preflight["Runner build or embedded TaskRef<br/>policy mapping<br/>prepare submission"]
    RuntimeLock["Runtime operation lock"]
    Current2{"Still current?"}
    Previous{"Previous binding?"}
    Cancel["Cancel and settle previous runtime"]
    Current3{"Still current?"}
    Bind["Bind Taskvisor ID<br/>create output channel"]
    Submit["submit_and_watch"]
    Accepted["Reconciled=True<br/>completion waiter"]
    Failed["Reconciled=False"]
    Stale["Return current state"]

    Desired --> Current1
    Current1 -->|no| Stale
    Current1 -->|yes| Preflight
    Preflight -->|failure| Failed
    Preflight --> RuntimeLock
    RuntimeLock --> Current2
    Current2 -->|no| Stale
    Current2 -->|yes| Previous
    Previous -->|yes| Cancel
    Previous -->|no| Current3
    Cancel -->|failure| Failed
    Cancel --> Current3
    Current3 -->|no| Stale
    Current3 -->|yes| Bind
    Bind -->|stale| Failed
    Bind --> Submit
    Submit -->|failure| Failed
    Submit -->|accepted| Accepted
```

Preflight runs outside the runtime operation lock.
Runner construction is an owned async future. One atomic admission coordinator
reserves the outer build's global and per-runner slots together. Nested catalog
builds reuse that global slot and reserve only their selected runner's slot.
Nested waiters have progress priority over roots. An outer build does not
deadlock behind a root waiting for its global slot. The build deadline starts
before outer admission and includes nested admission waits.
One coordinator per task retains at most one active and one pending request.
A newer request cancels active preflight and replaces the pending request.
Policy mapping and Taskvisor preparation happen only after that build returns.
Runner panics are contained and become `Reconciled=False`.

The runtime operation lock serializes replacement, cancellation, deletion, and binding by name.
It is separate from the desired operation lock.

Reconciliation uses latest-wins semantics.
A stale generation cannot acquire a new binding.

A bound generation can submit while a newer apply commits.
A later successful reconciliation replaces that runtime.
Accepted side effects are not rolled back.

The crate does not provide staged rollout or availability guarantees.

## Runtime identity

Model identity and Taskvisor identity are separate.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Resource["Task<br/>name + UID + generation + workload GVK"]
    Binding["RuntimeBinding"]
    Runtime["Taskvisor TaskId"]
    ByName["name to current binding"]
    ByRuntime["Taskvisor ID to exact binding"]

    Resource --> Binding
    Runtime --> Binding
    Binding --> ByName
    Binding --> ByRuntime
```

`TaskState` keeps both indexes under one write lock.
Binding a new generation removes the previous runtime index.

Taskvisor events resolve through the runtime ID.
State transitions then check the resource UID and generation.

An old generation can close its own retained run.
It cannot mutate the current generation's status.

Deleting and recreating one name produces a new UID.
Events for the old UID cannot mutate the new resource.

## Events and final outcomes

Taskvisor events and direct completion outcomes have different roles.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Runtime["Taskvisor"]
    Events["Best-effort subscriber events"]
    Observer["RuntimeObserver"]
    Attempt["Task status<br/>TaskRun detail<br/>output markers"]
    Waiter["Direct TaskWaiter outcome"]
    Final["Authoritative final state<br/>binding cleanup"]

    Runtime -.-> Events
    Events -.-> Observer
    Observer -.-> Attempt
    Runtime --> Waiter
    Waiter --> Observer
    Observer --> Final
```

Events provide attempt detail.
They can be dropped by the bounded subscriber path.

The direct `TaskWaiter` outcome owns final resource completion.
It does not depend on terminal event delivery.
It does not provide persistence across process termination.

`TaskRemoved` is a FIFO barrier for queued attempt events.
Finalization waits for that barrier for at most one second.
Subscriber overflow releases finalizations that are safe without the barrier.

The lifecycle gate serializes short event, completion, and management commits.
It is not held while waiting for Taskvisor.

Typed `TaskOutcomeKind` and `RejectionKind` values select terminal phases.
Free-form reason text remains diagnostic.

## State and collections

`TaskStateInner` holds all authoritative in-memory indexes.

| State                                  | Purpose                                             |
|----------------------------------------|-----------------------------------------------------|
| `tasks`                                | Current resources by model task name                |
| `by_slot`                              | Task names grouped by slot                          |
| `runs`                                 | Active and retained attempt history                 |
| `by_tv` and `tv_of`                    | Bidirectional runtime bindings                      |
| `finished_attempt_by_tv`               | Duplicate terminal event fencing                    |
| `resource_version_epoch` and counter   | Store-local collection identity                     |
| `watch_history`                        | Changes for snapshots, continuations, and replay    |
| `compacted_through`                    | Oldest unavailable collection revision              |
| `terminal_since`                       | Internal retention timestamps                       |
| `max_retained_tasks`                   | Optional current Task count limit                    |
| `retained_task_manifest_bytes`          | Aggregate current TaskManifest JSON bytes            |
| `retained_task_manifest_bytes_by_name`  | Per-task TaskManifest byte accounting                |
| `max_retained_task_manifest_bytes`      | Optional aggregate TaskManifest byte budget          |
| `watch_admission`                       | Concurrent watch and retained replay byte ledger     |

One `RwLock` protects the complete state.
A resource mutation and its change-journal entry happen under the same write lock.

An optional state sink receives the committed task snapshot or run value through a bounded FIFO dispatcher shared by all `TaskState` clones.
Each production write path declares its maximum event count and atomically reserves that many slots before acquiring the state lock.
Admission is FIFO and does not partially acquire a reservation.
Dropping the write guard releases the state lock before marking its batch ready; the publisher never crosses an unready queue front.
Application code therefore runs on one dedicated persistence worker and never under the state lock.
For configured capacity `C`, `reserved + buffered + active <= C + 1`; `active` is zero or one and every admitted event owns exactly one permit.
The largest atomic commit contains three events: one task change, at most one implicitly closed active run from the same generation, and one current run change.
The minimum configured capacity is therefore two buffered events, for a hard bound of three including the active callback.
Unused reservations return only after the state lock is released.
When the bound is reached, a writer waits before entering its state critical section.
Retention sweeps publish each expired task deletion as a separate revalidated commit batch.
Run events carry both task name and resource UID.
The sink must eventually return and must not mutate `TaskState`, directly or through a thread it waits for; reads are allowed.
Polling `SupervisorApi::shutdown` on the state callback worker panics before
shutdown changes state. A state callback must not wait for another thread that
calls shutdown; that cycle can deadlock.
`SupervisorApi::state_persistence_status` reports accepting, sticky health,
reserved/buffered/active ownership, the hard `C + 1` capacity, callbacks that
returned, and callbacks that panicked. A panicking callback is not retried
because its side effects are ambiguous. Later events continue through the worker.
Hooks do not hydrate the store during startup.

Resource versions contain a random store epoch and a monotonic revision.
They are opaque outside `TaskState`.
A version from another store is expired.
Counter exhaustion derives a new opaque epoch, clears the previous-epoch
journal, restarts the counter, and expires outstanding versions and watches.

### Snapshot pagination

The first query page captures the current collection resource version.
A continuation reconstructs that same snapshot from retained changes.

Filtering runs before pagination.
Items are ordered by task name.
Pagination keeps a complete Task prefix within count and serialized-item byte ceilings.
An oversized first Task is returned alone for native transport measurement.

The continuation carries the filter, snapshot version, and last returned name.
Changing the filter invalidates the continuation chain.

### Watches

No resource version or `"0"` emits a sorted `Added` snapshot.
An exact retained version replays later changes before live delivery.

Watch admission atomically reserves one concurrent lease and the compact Task
JSON bytes required by its initial or exact-resume buffer. The defaults are 256
leases and 64 MiB across one state. Bytes return to the ledger when a buffered
event is yielded. Dropping a watch, a terminal error, and shutdown release the
remaining lease without evicting another watch.

Lag recovery retains only a revision and an event probe across a pending poll.
It waits on the same byte ledger and re-reads the journal after wakeup. The
capacity check and waker registration share one mutex critical section. State
locks are released before predicates, byte measurement, admission, and wakeup.
History invalidation replaces an identity token after releasing the state lock,
then wakes admission waiters. Recovery compares the token in the same critical
section that registers its waker. Compaction before registration is therefore
observed as a token change, while compaction after registration wakes the
stream. Compaction during the wait produces the existing expired-version
error. One event larger than the complete byte budget is transferred directly
in `Poll::Ready` and is not retained internally.

Resource revisions do not all produce Task changes. After replaying retained
changes through a captured recovery target, a watch advances across a trailing
revision gap without emitting an event. A pending exact event probe that
disappears from retained history still expires the watch.

List continuations and watches share one change journal.
The journal is bounded by change count and serialized task bytes.
For a journal capacity above one, the live broadcast ring is the largest power
of two strictly below the journal capacity. This leaves count headroom when
real lag is reported. It does not reserve byte headroom: the independent byte
budget may compact required changes or retain none. A journal capacity of one
remains valid and has no count headroom for lag recovery.

An oversized change is delivered to current live subscribers.
It is not retained for replay.
Older resume points become compacted.

Adapter predicates participate in collection semantics.
They run before pagination.
They also classify watch visibility changes as `Added`, `Modified`, or `Deleted`.

Core does not hide embedded workloads.
Transport visibility belongs to the adapter.

## Runs and output

Attempt events create and finish `TaskRun` values.
Runs are ordered by generation and attempt when read.
Live run state and reversible journal deltas share immutable `Arc<TaskRun>`
snapshots. Mutations use `Arc::make_mut`. Query planning clones only shared
handles under the state lock and clones model values only for admitted page items.

A run snapshots its workload GVK.
Adapter filtering can therefore apply to historical generations.

The output hub may own one admitted bounded broadcast ring per bound task name.
Runners receive an `OutputSink`.
Consumers receive an `OutputSubscription`.
Core makes one bounded ownership copy from each borrowed runner chunk.
A small view cannot retain a larger producer allocation in the live ring.
One aggregate payload ledger defaults to 256 MiB. Each ring reserves exactly
`effective_capacity * max_chunk_bytes`. Each subscription separately reserves
`max_chunk_bytes` for the one internal event it can hold behind a lag notice.
A task continues without a live-output channel when its ring cannot be admitted.
A subscription returns `None` when its pending-event allowance cannot be admitted.
The ledger excludes events already yielded to callers and output-sink delivery copies.

Output is live-only and lossy.
Oversized chunks retain their exact prefix and set `truncated = true`.
A slow subscriber receives `OutputEvent::Lagged { skipped, skipped_bytes }`
and continues; the byte count covers retained chunk payloads that were lost.

An optional output sink receives published chunks and run markers, including the first event.
The event carries both task name and resource UID so output from different incarnations stays distinguishable.
Live broadcast happens before output callback-copy admission.
The sink runs on one dedicated worker behind a separate hard event-count bound.
The default bound is 2048 accepted events, including the active callback.
Runner publication never waits for output callback capacity or an admission lock.
A full, closed, contended, or unhealthy dispatcher drops only the callback copy;
task execution and live delivery continue.
`SupervisorApi::output_persistence_status` reports accepting, sticky health,
buffered and active ownership, hard capacity, callbacks that returned, callbacks
that panicked, and callback copies rejected by admission.
A callback panic is not retried, makes health false, and closes new admission.
The worker continues draining events accepted before that panic.
Polling `SupervisorApi::shutdown` on the callback worker panics before shutdown
changes state and is handled as a callback panic. A callback must not wait for
another thread that calls shutdown; that cycle can deadlock.
Shutdown closes admission after runtime tasks drain, then drains and joins the worker.
The callback has no SDK-owned retry or durability guarantee.
Subscriber-local `Lagged` notifications are not sent to the sink.

Terminal cleanup removes the channel from the hub.
Existing subscribers close after every outstanding sink clone releases its sender.
A stale sink cannot publish into a channel created later for the same model name.
Stale sinks and subscriptions retain the old ring's aggregate reservation.

## Retention

The retention worker runs inside the reconciler task tracker.
It uses `StateConfig::sweep_interval`.

One sweep:

1. removes expired finished runs;
2. removes expired unfinished runs without a runtime binding;
3. enforces the completed-run cap while keeping active runs;
4. removes terminal tasks after their run history is empty and `task_ttl` expires.

TaskRun pagination has its own resource-version epoch and reversible journal.
One committed state mutation uses one run revision, even when it closes an
active run, creates the next attempt, and evicts completed history together.
Continuations bind the snapshot to Task name and UID.
The journal is bounded by mutation-batch count and serialized bytes.
Counter exhaustion derives a new TaskRun epoch, clears the old journal, and
expires continuations from the previous epoch.

A task with a runtime binding is never removed by retention.
Only actual deletion or sweep removal releases retained Task count capacity.
Deletion, sweep removal, and shrinking applies release retained TaskManifest
byte capacity.
`StateConfig::with_max_retained_tasks(None)` disables the count limit.
`StateConfig::with_max_retained_task_manifest_bytes(None)` disables the
TaskManifest byte limit. Neither limit evicts Tasks.
The byte budget does not bound total process memory.

`max_runs_per_task = 0` keeps active runs and removes completed history.
Zero `run_ttl` and `task_ttl` make eligible values removable on the next sweep.
Zero `sweep_interval` is invalid.

## Concurrency

The crate uses three coordination layers.

| Layer                    | Key or scope | Protects                                                         |
|--------------------------|--------------|------------------------------------------------------------------|
| Desired operation locks  | Task name    | Writes, adapter checks, cancellation, and deletion admission     |
| Runtime operation locks  | Task name    | Binding, replacement, Taskvisor cancellation, and output binding |
| Lifecycle gate           | Global       | Short event, completion, deletion, and cleanup state commits     |

Both keyed lock maps store weak references.
Stale entries are pruned when a new lock is created.

When an operation needs both keyed locks, it acquires the desired operation lock first.
Reconciliation acquires only the runtime operation lock.

The spawn gate is separate.
It orders reconciliation worker registration against shutdown.

## Shutdown

Explicit shutdown follows one ordered path.

```mermaid
%%{init: {"flowchart": {"curve": "linear"}}}%%
flowchart LR
    Start["shutdown"]
    Fence["Set shutdown flag<br/>hold spawn gate"]
    Watches["Close task watches"]
    Retention["Cancel retention worker<br/>and runner builds"]
    Tracker["Close worker tracker"]
    Runtime["Shutdown Taskvisor"]
    Drain["Wait for reconciliation,<br/>completion, and retention workers"]
    Result["Return Taskvisor shutdown result"]

    Start --> Fence
    Fence --> Watches
    Watches --> Retention
    Retention --> Tracker
    Tracker --> Runtime
    Runtime --> Drain
    Drain --> Result
```

Desired writes fail with `CoreError::ShuttingDown` after the shutdown flag is set.
Read methods remain available over retained state.

Dropping `SupervisorApi` starts the same cleanup work on the captured Tokio runtime.
Drop cannot await or return the cleanup result.
Call `shutdown` when completion must be observed.

## Where to make a change

| Change                                  | Start here                                                                                                           | Verify here                                      |
|-----------------------------------------|----------------------------------------------------------------------------------------------------------------------|--------------------------------------------------|
| Public API or operation semantics       | [`src/supervisor/mod.rs`](src/supervisor/mod.rs), [`src/lib.rs`](src/lib.rs)                                         | supervisor tests and README doctests             |
| Builder setting or runtime assembly     | [`src/supervisor/builder.rs`](src/supervisor/builder.rs)                                                             | builder tests                                    |
| Desired write or precondition behavior  | [`src/state/mod.rs`](src/state/mod.rs), [`src/supervisor/mod.rs`](src/supervisor/mod.rs)                             | state and supervisor tests                       |
| Query, continuation, or watch semantics | [`src/state/mod.rs`](src/state/mod.rs)                                                                               | state collection tests                           |
| Runner routing or runtime intake        | [`src/runtime/reconciler.rs`](src/runtime/reconciler.rs)                                                             | reconciler behavior in supervisor tests          |
| Event, outcome, or cleanup projection   | [`src/runtime/observer.rs`](src/runtime/observer.rs)                                                                 | observer tests and `tests/taskvisor_contract.rs` |
| Taskvisor policy or phase mapping       | [`src/map/`](src/map)                                                                                                | mapper tests and `tests/taskvisor_contract.rs`   |
| Live output channels                    | [`src/output.rs`](src/output.rs)                                                                                     | output tests                                     |
| Persistence sink contracts              | [`src/persistence.rs`](src/persistence.rs), [`src/state/mod.rs`](src/state/mod.rs), [`src/output.rs`](src/output.rs) | persistence, state, and output tests             |
| Retention defaults or validation        | [`src/config.rs`](src/config.rs)                                                                                     | config and sweep tests                           |
| Public errors                           | [`src/error.rs`](src/error.rs)                                                                                       | error tests and every affected API test          |
| User-facing usage                       | [`README.md`](README.md), [`src/lib.rs`](src/lib.rs)                                                                 | `cargo test -p solti-core --doc --all-features`  |
