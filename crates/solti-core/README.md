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

    let task_ref = TaskFn::arc(|_ctx: TaskContext| async move {
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
| `query_task_runs`                        | `TaskId` and `TaskRunQuery`                | Snapshot-consistent `TaskRunPage`                |
| `subscribe_output`                       | `TaskId`                                   | Optional live `OutputSubscription`               |
| `cancel_task`                            | `TaskId`                                   | Runtime cancellation while desired state remains |
| `delete_task`                            | `TaskId`                                   | Logical runtime cancellation and resource removal |
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
It is asynchronous, cancellation-aware, deadline-bound, and admitted through
global and per-runner concurrency limits.

Embedded submission requires `TaskWorkload::Embedded`.
The caller supplies the matching `TaskRef`.
Embedded tasks bypass `RunnerRouter`.

Passing a routed workload to an embedded method is rejected.
Passing an embedded workload to a routed method is also rejected.

## Create and apply

`create_*` rejects every retained resource with the same `metadata.name`.
This includes terminal resources waiting for retention.
Duplicate-name rejection takes precedence over retained Task admission.

The default state limits are 1024 current Tasks and 256 MiB of aggregate
retained TaskManifest bytes. Every current Task counts toward cardinality,
including embedded, pending, running, and terminal Tasks. The byte budget sums
only each current Task's compact canonical `TaskManifest` JSON. The two budgets
are independent.

At the count limit, create and an unchecked apply for a missing name return
`CoreError::RetainedTaskLimitReached`. Applying an existing Task remains
allowed by the count limit. A create, missing apply, or positive-growth
existing apply returns `CoreError::RetainedTaskManifestByteLimitExceeded` when
it would exceed the TaskManifest byte budget. Shrinking and no-op applies remain
allowed. Admission and the desired-state write are atomic. Core does not evict
a Task or wait for capacity.

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
`ReconciliationScheduled` means the generation has not reached Taskvisor intake.
`TaskvisorOwnershipAndControllerIntakePending` means core is waiting on
Taskvisor's combined ownership and controller command-intake path. Taskvisor
0.9 exposes that path as one future. This condition therefore does not claim
which of those two capacities is currently blocking.
`task.taskvisor_intake_wait_started` and
`task.taskvisor_intake_wait_finished` tracing events carry the same scope,
Taskvisor ID, outcome, and elapsed wait time.

Reconciliation uses latest-wins semantics.
A task keeps at most one active and one pending reconciliation. A newer commit
cancels active runner preflight and replaces an older pending generation.
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
Pages keep a complete Task prefix within the query count limit and a 4 MiB serialized-item budget.
The byte budget can return fewer Tasks than the count limit.
An oversized first Task is returned alone for native transport measurement.

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

One state admits at most 256 concurrent watches by default. Initial snapshots
and exact-resume replay buffers share a 64 MiB aggregate compact Task JSON
budget. A new watch is rejected atomically when either limit is full. Existing
watches are not evicted or terminated by watch admission pressure.

Buffered initial and exact-resume bytes are released as events are yielded.
Live delivery retains only one coalesced revision notification. Each watcher
reads its next payload lazily from the shared count- and byte-bounded journal.
The cursor locates its next retained revision with a binary search.
It retains no private live-event ring or replay payload. If compaction removes
the next required revision, including when one change exceeds the complete
journal byte budget, the watcher terminates with an expired resource version.
Events already transferred to a caller are outside internal retention budgets.

`watch_history_capacity` grows the shared journal lazily. It never sizes an
eager live-delivery allocation. The independent byte budget remains a strict
upper bound for serialized Task payload retained by that journal.

Resource versions can contain revisions that did not publish a Task change.
After replaying every retained change through a coalesced notification target, a
watch advances across a trailing revision gap without inventing an event.

List continuations and watches share retained change history.
Compacted or foreign versions return `CollectionError::ResourceVersionExpired`.
Malformed or future versions return `CollectionError::InvalidResourceVersion`.
Counter exhaustion rotates the opaque collection epoch and expires versions
from the previous epoch.

`query_tasks_where()` and `watch_tasks_where()` add an adapter-owned visibility predicate.
Core itself does not hide embedded workloads.
A network adapter can filter them before pagination and watch transition classification.

## Run history

`query_task_runs()` returns a bounded snapshot page ordered by generation and attempt.
The default page size is 100 and the maximum is 1000.
The continuation fixes the Task name, UID, run revision, and last returned run.
It remains valid across Task deletion or recreation while its run journal is retained.
Each run snapshots the workload GVK of its generation.
A first-page query for an unknown task returns `None`.

TaskRun snapshots use a separate revision epoch and reversible journal.
The journal records creation, terminal updates, cap eviction, sweep removal,
and Task deletion. Compacted or foreign versions return
`CollectionError::ResourceVersionExpired`.
TaskRun counter exhaustion rotates its opaque epoch and expires earlier versions.
Core shares immutable run snapshots between live state and the reversible journal.
A mutation uses copy-on-write. A query clones shared handles under the state lock,
then clones only the model values admitted to its result page.

Attempt events provide run detail.
The direct Taskvisor outcome supplies the authoritative resource-level completion.
This second path can finalize a task when a best-effort event was dropped.
If state persistence admission has already closed, core cannot commit a new
terminal Task or TaskRun value. It still removes the exact runtime binding,
evicts only the matching UID output channel, and wakes cleanup waiters. The last
admitted Task and TaskRun values remain unchanged.

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
            OutputEvent::Lagged {
                skipped,
                skipped_bytes,
            } => {
                eprintln!("skipped {skipped} output events ({skipped_bytes} bytes)");
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
Empty channels and subscriptions reserve no payload bytes. Core charges the
aggregate budget only for payload currently retained by a ring or an internal
post-lag event. One shared payload has one charge even when several subscribers
can still read it. If a published chunk cannot be charged, the live stream
omits it and reports the exact event and payload loss through `Lagged`.
The aggregate budget does not include events already yielded to callers or
copies queued for an external output sink.

Terminal cleanup blocks new subscriptions.
Existing subscriptions close after every outstanding runner sink clone is dropped.
A stale sink remains attached to the old channel when a task name is reused.
Stale subscriptions retain only payload they can still read. The charge is
released when core drops its final owner.

## Cancel and delete

`cancel_task()` stops current scheduled reconciliation or requests a terminal
logical outcome for the current bound or queued runtime.
It keeps the desired resource and run history.
After the cancel worker is registered under the supervisor spawn gate, dropping
the caller future stops only that caller's wait. The worker remains in the
supervisor task tracker, and shutdown drains it.
A cancel call that reaches the gate after shutdown closes operation admission
returns `CoreError::ShuttingDown` and registers no worker.
It first cancels and drains core-owned reconciliation for the task name.
Dropping a `PreparedSubmission` before Taskvisor intake starts no Taskvisor work
and releases the cancellation-safe ownership wait.
Core keeps the caller `TaskRef` in guarded ownership before preparation. It
retains a separate anchor while a clone is inside preparation or intake.
Runtime sources, prepared submissions, intake futures, and the final anchor use
a non-unwinding disposal boundary. A destructor panic and a nested panic-payload
destructor cannot strand cancellation or shutdown. Cleanup reporting is also
best effort and never reports the panic payload. This boundary does not make a
blocking destructor bounded.
The runtime observer returns an exact provisional-binding guard as soon as both
binding indexes are installed. An unexpected reconciliation unwind removes only
that binding and its UID-matched output channel before coordinator settlement.
Core creates no `TaskRun`. The retained task stays `Pending` with attempt zero and
`Reconciled=False/RuntimeSubmissionCancelled`. Applying the same manifest may
retry that failed reconciliation.
If shutdown's preflight stop reaches the coordinator first, the retained task
keeps its existing `Reconciled=Unknown/TaskvisorOwnershipAndControllerIntakePending`
diagnostic instead. This path also creates no `TaskRun`.
If controller intake won the race, core delegates queued or running cancellation
to Taskvisor by the exact prepared ID and waits for Taskvisor's authoritative
outcome. Core registers the returned completion waiter before tracing or waiting
for observed-state persistence. A full persistence queue therefore cannot keep
the coordinator from reaching native exact-ID cancellation.
An unknown name returns `CoreError::NotFound`.

`cancel_task_with_preconditions()` rejects a missing resource and stale guards.
`cancel_task_where()` also applies an adapter-owned visibility predicate under
the same per-name operation lock.

`delete_task()` waits for that logical outcome and removes the resource and its runs.
A Taskvisor `ForceAborted` outcome does not prove physical exit of non-cooperative task code.
Deleting a missing resource is an idempotent no-op.

`delete_task_with_preconditions()` rejects a missing resource and stale guards.
`delete_task_where()` also applies an adapter-owned visibility predicate under the same per-name operation lock.

## Configuration

`StateConfig` controls in-memory retention:

| Field                              | Default      | Meaning                                                    |
|------------------------------------|--------------|------------------------------------------------------------|
| `run_ttl`                          | 1 hour       | Age limit for terminal runs and unbound orphaned runs      |
| `task_ttl`                         | 1 hour       | Age limit for terminal tasks after run history is empty    |
| `sweep_interval`                   | 5 minutes    | Retention worker interval                                  |
| `max_runs_per_task`                | 256          | Per-task completed run cap                                 |
| `max_retained_tasks`               | 1024 tasks   | Maximum current Task resources                             |
| `max_retained_task_manifest_bytes` | 256 MiB      | Maximum aggregate compact TaskManifest JSON bytes          |
| `max_retained_task_run_bytes`      | 256 MiB      | Maximum aggregate compact JSON for current TaskRun values  |
| `max_concurrent_task_watches`      | 256 watches  | Maximum admitted Task watch subscriptions                  |
| `max_task_watch_initial_replay_bytes` | 64 MiB    | Aggregate compact Task JSON in initial and replay buffers  |
| `run_history_capacity`             | 4096 batches | Maximum retained TaskRun mutation batches                  |
| `run_history_byte_budget`          | 64 MiB       | Maximum compact JSON bytes for run identities and values   |
| `watch_history_capacity`           | 4096 changes | Maximum retained collection-journal changes                |
| `watch_history_byte_budget`        | 64 MiB       | Maximum serialized task bytes in collection change history |

`max_runs_per_task = 0` removes completed history while keeping active runs.
All current Tasks count toward `max_retained_tasks`, including terminal and
embedded Tasks. Its type is `Option<NonZeroUsize>`, with `Some(1024)` as the
default. `with_max_retained_tasks(None)` disables the count limit, while
`try_with_max_retained_tasks(0)` is rejected.

`max_retained_task_manifest_bytes` is also an `Option<NonZeroUsize>`, with
`Some(256 MiB)` as the default.
`with_max_retained_task_manifest_bytes(None)` disables this byte budget, while
`try_with_max_retained_task_manifest_bytes(0)` is rejected. It measures only
compact canonical `TaskManifest` JSON. It does not bound total process memory.

`max_retained_task_run_bytes` is an independent `Option<NonZeroUsize>`, with
`Some(256 MiB)` as the default. It counts each TaskRun currently present in
query state once. When a run mutation exceeds the bound, core uses a maintained
completion-time index to compact the globally oldest completed runs. If active
values alone cannot fit a deliberately smaller custom budget, execution and
`TaskStateSink` lifecycle delivery continue while the new active value is
omitted from retained query state. A lifecycle-only active handle retains the
observed attempt start. Direct completion still publishes its authoritative
terminal `RunChanged` event without forcing the value into query retention.
This budget does not include reversible run journal deltas.
`with_max_retained_task_run_bytes(None)` disables it and the raw checked setter
rejects zero.

All serialized byte budgets are logical compact JSON payload bounds. They do
not measure Rust allocation overhead or process RSS.

The two Task watch limits also use `Option<NonZeroUsize>`.
`with_max_concurrent_task_watches(None)` and
`with_max_task_watch_initial_replay_bytes(None)` disable their respective
limits. Their raw checked setters reject zero. The watch byte budget measures
compact Task JSON retained by internal initial and replay buffers. It excludes
queue metadata, the shared change journal, live delivery, and caller-owned
yielded events. It is not a total process-memory limit.
`sweep_interval = 0` is rejected. Forward deadlines are limited to 30 years,
so `sweep_interval` values above that ceiling are also rejected. `run_ttl` and
`task_ttl` are elapsed ages rather than forward deadlines and keep accepting
the full `Duration` range.
Watch history capacity and byte budget must also be greater than zero.

`ReconciliationConfig` bounds routed runner construction:

| Field                              | Default    | Meaning                                      |
|------------------------------------|------------|----------------------------------------------|
| `build_timeout`                    | 30 seconds | Deadline for root admission and runner build |
| `max_concurrent_builds`            | 32         | Concurrent outer routed-build limit          |
| `max_concurrent_builds_per_runner` | 8          | Limit for each selected runner, nested too   |

These defaults are SDK policy values, not a claim of a benchmark-derived
optimum. Override them with values measured for the application's runner
workloads and service objectives.

One global build slot covers the outer build and every nested catalog build it
creates. Nested builds acquire only the selected runner's per-runner slot. The
build deadline starts before outer admission and includes waits for nested
runner slots.

All reconciliation limits must be greater than zero. `build_timeout` is also
limited to the same 30-year forward-deadline ceiling. Embedded tasks do not
use runner-build admission because the caller supplies their `TaskRef`.

`OutputConfig` controls best-effort live output. Defaults are 256 events, a
16 MiB retained payload budget per task, 64 KiB per chunk, and a 256 MiB
aggregate retained payload budget. The effective ring capacity is the stricter
of the per-task event and byte limits. Ring storage starts empty and grows only
with events published while at least one subscriber exists. Creating a task
channel does not allocate the configured event capacity, and output published
without subscribers is not retained. The aggregate ledger charges actual
retained chunk bytes. Shared payload is charged once across all subscribers.
This is not a total process memory limit; caller-owned yielded events and
output-sink delivery have separate ownership.

Core makes one ownership copy of every retained chunk into bounded storage. A
custom runner cannot retain an oversized or hidden backing allocation through
a small `Bytes` view.
Oversized chunks carry the exact retained prefix with `truncated = true`. A
slow subscriber receives `Lagged` with the number of overwritten events and
their retained chunk payload bytes in `skipped_bytes`.
Zero limits and a chunk limit larger than the per-task byte budget are rejected.

```rust,no_run
use std::time::Duration;

use solti_core::{OutputConfig, ReconciliationConfig, StateConfig, SupervisorApi};
use solti_runner::RunnerRouter;

async fn configured() -> Result<(), Box<dyn std::error::Error>> {
    let state = StateConfig::new()
        .with_run_ttl(Duration::from_secs(10 * 60))
        .with_task_ttl(Duration::from_secs(30 * 60))
        .try_with_sweep_interval(Duration::from_secs(60))?
        .with_max_runs_per_task(64)
        .try_with_max_retained_tasks(1_024)?
        .try_with_max_retained_task_manifest_bytes(64 * 1024 * 1024)?
        .try_with_max_retained_task_run_bytes(64 * 1024 * 1024)?
        .try_with_max_concurrent_task_watches(128)?
        .try_with_max_task_watch_initial_replay_bytes(32 * 1024 * 1024)?
        .try_with_watch_history_capacity(1024)?
        .try_with_watch_history_byte_budget(16 * 1024 * 1024)?;
    let output = OutputConfig::try_new(1024)?
        .try_with_byte_limits(64 * 1024 * 1024, 64 * 1024)?
        .try_with_aggregate_byte_budget(512 * 1024 * 1024)?;
    let reconciliation = ReconciliationConfig::new()
        .try_with_build_timeout(Duration::from_secs(20))?
        .try_with_max_concurrent_builds(16)?
        .try_with_max_concurrent_builds_per_runner(4)?;

    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_config(state)
        .with_output_config(output)
        .with_reconciliation_config(reconciliation)
        .start()
        .await?;

    api.shutdown().await?;
    Ok(())
}
```

The builder also accepts Taskvisor runtime configuration, controller configuration, and external Taskvisor subscribers.
The core state observer is always installed.
Taskvisor 0.9 charges configured subscribers and task values retained through
intake, queuing, physical execution, and isolated destruction against the same
`SupervisorConfig::ownership_capacity`. The core observer consumes one slot,
and every external subscriber consumes one more. This limit is separate from
`max_registered_tasks` and the SDK retained-task limit. A configuration that
cannot reserve all configured subscribers fails startup with
`CoreError::SupervisorInitialization`.

## Persistence hooks

Core can forward committed task state changes and task output to optional application-owned sinks.

The hooks are storage-neutral.
They do not add a database, replace the in-memory `TaskState`, retry delivery, or make state durable by themselves.
They are write-side notifications; core does not load persisted state during startup.

State events use a bounded, core-owned FIFO dispatcher.
A Tokio-owned writer asynchronously reserves its mutation path's maximum event count before acquiring the lifecycle, state, or spawn gate, records one atomic commit batch, and publishes its events only after releasing the state lock.
Taskvisor subscriber callbacks use the same fair semaphore from their dedicated callback workers, preserving one FIFO across both caller kinds without parking a Tokio worker.
They reserve before entering the same fair lifecycle gate. Metadata-only lifecycle
sections never wait for persistence capacity. `TaskRemoved` reserves one maximum
finalization batch; overflow rechecks and finalizes safe pending identities one
at a time instead of holding the gate across a variable-size reservation.
Unused reservations are returned after the lock is released.
One dedicated worker invokes the sink in commit order.
For configured capacity `C`, the hard bound is `reserved + buffered + active <= C + 1`, where `active` is zero or one.
Reserved includes permits owned by a commit before its event values enter the pending FIFO.
The minimum capacity is two buffered events because one attempt transition can atomically emit three events: one task change, one implicitly closed prior run, and one current run change.
Reservation admission is FIFO; saturation applies backpressure before the state critical section.
Canceling a waiting async writer removes its reservation and returns any provisional semaphore capacity.
Every eventful mutation first owns a sink-independent admission lease.
Shutdown closes that admission fence under the same mutex used to issue leases,
rejects later public writes with `CoreError::ShuttingDown`, and waits for every
earlier lease to commit or be dropped. A Taskvisor callback that arrives after
this boundary returns without mutating state, even when no state sink is configured.
When a state sink is configured, admission also clones the state-dispatch sender
before waiting for semaphore capacity. After the common mutation fence drains,
shutdown closes the dispatcher sender under its sender mutex and drains every
event accepted by an earlier admission before the worker exits.
Retention sweeps publish each expired task deletion as a separate commit batch.
State callbacks may read `TaskState`, but must not mutate it directly or wait for another thread that mutates it.
They may wait for unrelated Tokio work, provided the callback eventually returns.
Polling `SupervisorApi::shutdown()` on the state callback worker panics before shutdown starts.
A state callback must not wait for another thread that calls shutdown; that cycle can deadlock.
Output events use a separate bounded dispatcher and dedicated worker.
Its default hard bound is 2048 accepted `TaskOutputEvent` values, including the active callback.
Runner publication attempts callback-copy admission without waiting for capacity.
A full, closed, contended, or unhealthy dispatcher drops only that callback copy.
Task execution and the live output ring continue.
Both callback types and their sink destructors must eventually return so
shutdown can drain accepted events:

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
Each event carries the task name and UID.
A late event from a deleted task cannot be confused with a recreated task using the same name.
Subscriber-local `Lagged` notifications are not persisted.
Live broadcast happens before output callback-copy admission.
An output callback panic is caught and logged, is not retried, marks health false, and closes new admission.
The worker continues draining events accepted before an ordinary callback panic.
If destroying the callback or reporting panic payload itself panics, the worker
forgets that one replacement payload, stops invoking the sink, and drops its
remaining callback copies.
Polling `SupervisorApi::shutdown()` on the output callback worker panics before shutdown starts and is handled as a callback panic.
An output callback must not wait for another thread that calls shutdown; that cycle can deadlock.
`SupervisorApi::state_persistence_status()` exposes admission, sticky health,
outstanding event and payload ownership, their hard capacities, completed
callbacks, and panicked callbacks. State payload admission defaults to 256 MiB.
Writes first reserve a conservative mutation-class bound before lifecycle,
spawn, and state critical sections. After commit, that reservation shrinks to
the sum of its emitted variant charges. `TaskChanged` counts the concatenated
compact JSON of present previous and current Task values. It excludes the
event's separate `resource_version` value and all event-variant framing.
`RunChanged` counts the compact JSON of its task name, task UID, and TaskRun
values and excludes event-variant framing. Saturation applies lossless
backpressure. A panicking callback is not retried because its side effects are
ambiguous. Later state events continue after an ordinary callback panic. If
destroying the callback or reporting panic payload itself panics, the worker
forgets that one replacement payload, closes admission, drops its remaining
accepted events, and reports terminal worker failure. An internal state-worker
panic also marks the dispatcher unhealthy and closes new admission before later
callers can enter the state lock.
`SupervisorApi::output_persistence_status()` exposes accepting, sticky health,
buffered and active ownership, the hard capacity, completed callbacks, panicked
callbacks, and callback copies rejected by admission.
`PersistenceConfig::output_queue_capacity()` defaults to the hard bound 2048.
`try_with_output_queue_capacity` rejects zero.
`PersistenceConfig::state_queue_byte_capacity()` defaults to 256 MiB. Its
checked setter rejects zero, values below the largest atomic commit reservation,
and values above Tokio's semaphore limit. One Task change reserves 16 MiB. One
Run change reserves 196 KiB, including worst-case JSON escaping of the bounded
diagnostic and a standalone fixed-field allowance. The three-event maximum
reserves 16 MiB plus 392 KiB.
The application owns database errors and retries.
Core drains both persistence workers during shutdown.

## Shutdown

Call `shutdown()` to observe completion of the bounded Taskvisor and SDK-owned
cleanup workflow before the application continues.
Do not poll it on a persistence callback worker or wait there for another shutdown caller.

`shutdown` and `shutdown_with_timeout(duration)` join one cached SDK-owned
operation. Repeated or canceled callers do not create additional cleanup
owners. A timeout applies only to that caller. If it returns
`ShutdownTimedOut`, the shared coordinator remains detached and continues
draining. The deadline does not terminate a callback or task. This makes an
application callback that never returns observable without changing the
lossless `shutdown()` contract.

Shutdown closes task watches.
It cancels runner builds, stops Taskvisor, and waits for reconciliation,
completion, retention, and persistence workers.
State and output persistence use one common cleanup tail. Failure of either
worker is reported as `ShutdownCoordinatorStopped` only after both shutdown
paths have been attempted.
Each worker destroys its final application sink handle before publishing its
terminal outcome. Sink destructor panics, including a nested panic-payload
destructor, are contained and reported as worker failure rather than leaving
shutdown pending. A sink destructor must eventually return; a caller deadline
can observe a blocked destructor but does not terminate it.

Taskvisor 0.9 reports `ForceAborted` as a logical outcome. Task code that does
not cooperate with cancellation can remain physically active after shutdown
returns. The controller keeps its slot until that task ownership is physically
released.

Dropping `SupervisorApi` starts the same cleanup path in the background.
It does not provide an awaitable result.
Persistence dispatcher destructors close admission and detach their worker;
only explicit shutdown observes lossless drain completion.

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

| Variant                    | Cause                                                        |
|----------------------------|--------------------------------------------------------------|
| `StateInitialization`      | The state resource-version identity could not initialize     |
| `PersistenceInitialization` | A configured persistence worker could not start             |
| `SupervisorInitialization` | Taskvisor rejected supervisor construction                   |
| `ShuttingDown`             | A desired-state mutation raced with or started after shutdown |
| `ShutdownTimedOut`         | A caller-owned shutdown deadline elapsed before drain completed |
| `ShutdownCoordinatorStopped` | The SDK-owned shutdown coordinator stopped unexpectedly      |
| `Supervisor`               | Taskvisor start, prepare, submit, cancel, or shutdown failed |
| `AlreadyExists`            | Create found a retained resource with the same name          |
| `NotFound`                 | A required resource does not exist or is hidden              |
| `Conflict`                 | UID or resource-version preconditions failed                 |
| `Mapping`                  | A model policy has no Taskvisor mapping                      |
| `Runner`                   | Runner selection or task construction failed                 |
| `InvalidSpec`              | The submitted model or workload path is invalid              |

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

Process benchmarks live in the workspace-level [benchmark suite](../../benches/README.md).
They cover lifecycle, reconciliation, collections, output, and composed SDK processes.
