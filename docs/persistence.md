---
title: Persistence hooks
description: Forward committed state and live output through separate bounded sink contracts.
---

# Persistence hooks

Core can forward state changes and output to application-owned sinks. These hooks define delivery and shutdown boundaries; they do not provide a database, storage acknowledgement protocol, or automatic restoration of a supervisor.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Supplies the Task, TaskRun, UID, and OutputEvent values carried by callbacks. |
| `solti-core` | Reserves state-event ownership, serializes state delivery, dispatches output copies, exposes sink status, and drains workers. |
| `solti-runner` and concrete runners | Publish output independently of external callback capacity. |
| Application sink | Owns storage, serialization, durability, storage-error handling, and any recovery process. |

## Install the sinks before startup

`SupervisorApiBuilder::with_state_sink` and `with_output_sink` accept shared trait objects. `with_persistence_config` sets their dispatcher capacities. Core starts the dispatchers as part of supervisor startup.

The following wiring uses application-provided sink implementations:

```rust
use solti_core::{
    CoreError, PersistenceConfig, SupervisorApi,
    TaskOutputSinkHandle, TaskStateSinkHandle,
};
use solti_runner::RunnerRouter;

async fn start_with_sinks(
    router: RunnerRouter,
    state: TaskStateSinkHandle,
    output: TaskOutputSinkHandle,
) -> Result<SupervisorApi, CoreError> {
    SupervisorApi::builder(router)
        .with_persistence_config(PersistenceConfig::default())
        .with_state_sink(state)
        .with_output_sink(output)
        .start()
        .await
}
```

Both traits have a synchronous `on_event` callback returning `()`. A callback returning normally does not communicate a database commit, fsync, or replication acknowledgement to core. The application must define what it does inside that callback and how it reports storage failures.

Failure to start a persistence worker is a supervisor startup error, `CoreError::PersistenceInitialization`.

## State events: ordered delivery with backpressure

`TaskStateSink` receives:

| Event | Contents |
| --- | --- |
| `TaskStateEvent::TaskChanged` | Assigned resource version and optional previous/current `Arc<Task>` snapshots. Create has no previous value; delete has no current value. |
| `TaskStateEvent::RunChanged` | Task name, exact task UID, and the current `TaskRun` after a lifecycle change. |

Run retention does not publish run-delete events. This hook journals lifecycle changes; it does not mirror the current in-memory retention window. A removed task is visible through its Task change, not a separate delete callback for every run.

State mutations reserve bounded event-count and payload-byte ownership before entering the authoritative state critical section. Tokio-owned mutations await fair admission. Taskvisor callback workers use the same admission future on their dedicated threads.

The mutation commits, releases the state lock, and queues its accepted events. One dedicated worker calls the sink in commit order. A slow callback fills the queue and then backpressures later mutations; overload does not drop accepted state events.

The resource write does not wait for the external callback to finish. “Committed” in the core API means committed to core's state, not committed to external storage.

State callbacks must not mutate `TaskState`, directly or by waiting for another thread that does so. Reads and waits for unrelated Tokio work are allowed. This restriction prevents a callback from waiting on the admission capacity it is responsible for releasing.

## State capacity accounting

Default state delivery has a configured queue capacity of 2,048 plus one active callback, and a 256 MiB logical payload bound.

For a configured count `C`, the hard invariant is:

```text
reserved events + buffered events + active callback <= C + 1
```

The active callback count is zero or one. The public status `capacity()` includes that active slot.

Byte admission first reserves a conservative maximum for an atomic commit, then shrinks to the committed payload charge:

- `TaskChanged`: compact JSON bytes of the present previous and current Task values. The separate resource-version field and event framing are excluded.
- `RunChanged`: compact JSON bytes of TaskId, UID, and TaskRun values. Event framing is excluded.

Reservations, buffered events, and the active callback all count toward the byte bound. This is a logical serialized-payload bound, not allocator or RSS accounting. Caller-owned copies retained by a sink have their own lifetime outside the dispatcher bound.

Checked configuration rejects capacities unable to admit the largest atomic commit. The current minimum count is two buffered events plus the active slot; the minimum byte capacity is 16 MiB plus 392 KiB. Zero and values beyond the underlying semaphore limit are also rejected where applicable.

## Output events: separate best-effort copies

`TaskOutputSink` receives a `TaskOutputEvent` containing the task name, exact resource UID, and original `OutputEvent`. Core installs it before runners start, allowing callback delivery from the first publication without requiring a live subscriber.

The output dispatcher is independent from state persistence and live output buffering. Its default capacity is 2,048 events, including the active callback.

Runner publication never waits for callback capacity. A full, closed, contended, or unhealthy dispatcher drops the external callback copy. It does not stop the task or remove the independently published live-output copy.

Output sink loss is reported through `TaskOutputSinkStatus::dropped()`, not through a replay protocol. A live subscriber's `Lagged` event describes that subscriber's live-channel gap; it does not acknowledge or enumerate output-sink losses.

The output queue has an event-count admission bound. It is not included in `OutputConfig`'s aggregate live-ring payload budget. See [output and history](output-and-history.md) for truncation and live retention.

## Health and callback failures

Use `SupervisorApi::state_persistence_status()` and `output_persistence_status()`. Each returns `None` when its sink is not installed.

| Status field | Meaning |
| --- | --- |
| `accepting` | Dispatcher admission remains open. |
| `healthy` | No callback or worker panic has been observed. False is sticky. |
| `queued`, `capacity` | Outstanding accepted ownership and its hard count bound. State includes pre-write reservations; output includes buffered and active callbacks. |
| State `queued_bytes`, `byte_capacity` | Reserved or retained state-event payload and its bound. |
| `delivered` | Callbacks that returned normally, not independently verified storage success. |
| `failed` | Callbacks that panicked. |
| Output `dropped` | Callback copies rejected by admission. |

A callback panic has ambiguous side effects and is not retried.

The two workers respond differently:

- A state callback unwind marks health false and ordinarily continues with later events in order.
- An output callback unwind marks health false and closes new output-copy admission. The worker still drains copies accepted before that panic.

A worker failure or a secondary panic while disposing a caught/reporting payload can terminally disable a worker. Core contains sink destruction through a non-unwinding boundary; a sink-destructor panic makes shutdown fail instead of leaving the shared outcome pending.

These defenses do not make arbitrary sink code safe to block forever or provide transactional retry semantics.

## Shutdown is the drain boundary

Supervisor shutdown first settles runtime and SDK work, then drains accepted state and output events. State reservations that crossed admission before normal shutdown remain accepted and drain with the worker. A failed worker can instead reject pending reservations that did not complete admission.

Callbacks and sink destructors must eventually return. `shutdown_with_timeout` can bound the caller's wait, but the coordinator continues and does not terminate a callback.

Do not poll supervisor shutdown from inside either sink's `on_event`; core rejects that reentry with a panic before shutdown begins. Sink destructors run outside that callback guard and must not await shutdown either. Do not wait for another thread that calls shutdown, because shutdown would be waiting for the callback or destructor itself.

Dropping dispatcher owners closes admission and detaches their thread handles. It does not synchronously observe lossless drain. Use explicit `SupervisorApi::shutdown` when the process needs that boundary.

## Examples and source

- [Routed output example](../crates/solti-core/examples/routed_output.rs): the complete runner and supervisor flow into which the builder hooks fit.
- [Builder hook contracts](../crates/solti-core/src/supervisor/builder.rs) and [sink, status, queue, and shutdown implementation](../crates/solti-core/src/persistence.rs).
- [State commit integration](../crates/solti-core/src/state/mod.rs) and [output-copy dispatch](../crates/solti-core/src/output.rs).

## See also

- [Managing tasks](managing-tasks.md) for the in-memory commit boundary.
- [Output and history](output-and-history.md) for source data and loss boundaries.
- [Cancellation and shutdown](cancellation-and-shutdown.md) for the full shutdown sequence.
