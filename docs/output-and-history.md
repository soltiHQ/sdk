---
title: Output and history
description: Separate current task state, retained attempt records, live output, and external persistence copies.
---

# Output and history

Task status, attempt history, and output answer different questions. None is a substitute for the other two.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Defines `TaskRun`, `OutputEvent`, byte chunks, stream identity, and run queries. |
| `solti-runner` | Gives runners a write-only output publisher and attempt-scoped sinks. |
| Concrete runner | Captures backend output and publishes bytes with the correct attempt identity. |
| Taskvisor | Produces attempt events and the direct managed-task outcome. |
| `solti-core` | Projects state and history, adds output lifecycle markers, owns bounded live channels, and dispatches optional persistence copies. |

## Choose the observation surface

| Surface | What it contains | Retention and delivery |
| --- | --- | --- |
| `Task.status` | Current generation's reconciliation and execution projection. | Retained resource state, updated by runtime observations and finalization. |
| `query_task_runs` | Attempt records, including older generations. | Bounded in-memory history and snapshot pagination. |
| `subscribe_output` | Newly published chunks and lifecycle markers. | Live-only, bounded, best-effort. No historical replay. |
| `TaskStateSink` | Committed Task and TaskRun changes. | Bounded, ordered state-event dispatch with admission backpressure. |
| `TaskOutputSink` | Output events with task name and exact UID. | Separate bounded best-effort callback copies. |

The sink contracts are described in [persistence](persistence.md). Installing an output sink does not turn a live subscription into a replayable log.

## Attempt records

A `TaskRun` records workload identity, generation, attempt, phase, start and finish timestamps, and optional error or exit code. Generation and attempt are positive. An active run is `Running` without a finish timestamp; a terminal run has a finish timestamp.

The owning task UID is supplied by the run page or persistence event, not stored inside each `TaskRun`. Use that UID together with generation and attempt when correlating a record across same-name resource recreation.

Core creates and updates runs from Taskvisor attempt observations. A task-level final event is not an extra attempt. The direct final outcome can settle an active lifecycle record and current resource even when attempt events were lost.

Attempt history is not a complete execution audit:

- Taskvisor event delivery is best-effort. Entire attempt details can be absent.
- If a finish is observed without a retained start, core uses local projection time for the missing start. It does not recover the actual execution start time.
- If a later attempt arrives before the prior outcome was observed, core closes the older active record with an explicit missing-outcome diagnostic.
- A logical finish timestamp does not prove physical exit after a force-abort.

`TaskRun` therefore supports retained lifecycle inspection, but its timestamps and record count are not guaranteed measurements of every physical attempt.

## History retention

| `StateConfig` setting | Default | Effect |
| --- | --- | --- |
| `max_runs_per_task` | 256 | Caps completed runs for a task. Zero removes completed history while retaining active lifecycle handling. |
| `max_retained_task_run_bytes` | 256 MiB | Caps compact JSON for runs present in current query state. |
| `run_ttl` | 1 hour | Makes finished runs eligible for removal by age. Unbound nonterminal runs can also expire. |
| `task_ttl` | 1 hour | Makes terminal resources eligible once they have no retained runs or runtime binding. |
| `sweep_interval` | 5 minutes | Cadence of the retention worker. TTL is eligibility, not an exact deletion deadline. |

Aggregate run-byte pressure compacts completed runs oldest-first across tasks. If active values alone cannot fit, a newly active run can be omitted from retained query state while execution and lifecycle delivery continue. Core keeps a lifecycle-only active handle for terminal projection.

Current-run retention and the reversible run journal have different budgets. A continuation can reconstruct a previous retained snapshot after a live record is removed, but only while its journal position survives. See [collections and watches](collections-and-watches.md).

Deleting a task removes its retained run history. Persistence `RunChanged` events are a lifecycle journal, not a mirror of the retention window; run retention does not emit run-delete callbacks.

## Publish output from a runner

Core installs an `OutputPublisher` in the router's `BuildContext`. A runner requests an `OutputSink` for the resource name, generation, and attempt, then sends stdout or stderr bytes through it. No available sink means output publication is disabled, not that execution must fail.

Request the sink from inside the attempt future before creating separate forwarding tasks. Composition output context is not automatically inherited by separately spawned readers. A runner can clone the acquired sink and pass that clone to its forwarders.

The sink is write-only. It cannot subscribe, close core channels, or change task lifecycle state. Its clones share sequence counters and callback-failure state.

Output callbacks execute synchronously on the publishing thread and must not block runner execution. A caught unwinding callback panic disables later calls through that sink; already-running concurrent calls may still finish. This containment does not suppress the process panic hook and does not work with `panic = "abort"`.

## Bytes, framing, and ordering

Each chunk has generation, attempt, stream, sequence, publication timestamp, raw line bytes, and a `truncated` flag.

- Stdout and stderr have independent sequence counters starting at zero. Cloned sinks share them; there is no single merged stdout/stderr sequence.
- LF terminates a chunk. CR immediately before LF belongs to the delimiter; other bytes remain exact.
- Empty input produces an empty chunk. A trailing delimiter does not add a second empty chunk; consecutive delimiters preserve intervening empty lines.
- Chunk bytes are not required to be UTF-8. Model JSON represents them as base64.
- Core retains a byte-prefix when a chunk exceeds its configured maximum and sets `truncated=true`.

`RunStarted` and `RunFinished` are best-effort markers derived from lifecycle observation. They are not output-ordering barriers. A chunk can arrive after `RunFinished`; correlate using its own generation, attempt, and stream sequence. Do not use a marker as a promise that all forwarded bytes have drained.

## Subscribe to a live channel

`subscribe_output(&name)` returns an `OutputSubscription` if an open channel is available. It starts at the subscription point. Existing buffered events from before the subscription are not replayed.

A subscription can be unavailable because no channel exists yet, the channel is closed, or its subscriber count cannot be represented. Desired-state commit alone does not guarantee that reconciliation has reached output-channel creation.

For an adapter, `subscribe_output_where(&name, &uid, predicate).await` atomically checks exact UID, visibility, current-generation runtime binding, and subscription under the per-name locks. It returns the bound generation with the stream. The predicate must be pure, non-blocking, and must not reenter `SupervisorApi`.

Terminal binding cleanup removes the channel from new subscription lookup. An existing stream can continue while a runner still owns a sink sender and closes after the final sender is released. Old sink handles retain their original broadcaster and UID; they are not retargeted to a recreated same-name resource.

`OutputEvent` itself does not carry the task UID. The caller's subscription context or the `TaskOutputEvent` persistence envelope supplies that identity.

## Bounded buffering and loss

`OutputConfig` defaults to 256 events per task, 64 KiB per chunk, a 16 MiB per-channel byte budget, and a 256 MiB aggregate retained-payload budget.

The effective ring capacity is a power of two rounded down to fit the requested count and the per-channel byte budget at the maximum chunk size. Empty channels reserve no aggregate payload. Ring storage is lazy and only retains live-delivery events while subscribers exist.

The aggregate bound charges retained chunk bytes once for a shared ring event, not once per subscriber. It does not include event metadata, allocation overhead, caller-owned chunks already yielded from a stream, or the separate output-persistence copy.

A slow subscriber can lose events when the ring advances. Aggregate payload admission can also omit a publication. The stream reports these gaps as `OutputEvent::Lagged { skipped, skipped_bytes }`, then continues with available newer events. The byte count describes omitted retained payload, not bytes already removed by source or core truncation.

Output loss does not fail or retry the task. If a process needs stored output, its application must define that storage behavior through the separate sink contract and account for that sink's own loss boundary.

## Examples and source

- [Routed output example](../crates/solti-core/examples/routed_output.rs): custom runner, live subscription, chunk identity, and retained history.
- [Output model](../crates/solti-model/src/domain/output.rs), [attempt record](../crates/solti-model/src/resource/run.rs), and [runner sink contract](../crates/solti-runner/src/output.rs).
- [Live output implementation](../crates/solti-core/src/output.rs), [runtime projection](../crates/solti-core/src/runtime/observer.rs), and [retained state](../crates/solti-core/src/state/mod.rs).

## See also

- [Collections and watches](collections-and-watches.md) for run pagination and resource watches.
- [Persistence](persistence.md) for external callback delivery and health.
- [Cancellation and shutdown](cancellation-and-shutdown.md) for stream closure versus cleanup completion.
