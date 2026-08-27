---
title: Collections and watches
description: Read stable task and run snapshots and follow filtered resource changes within bounded history.
---

# Collections and watches

Use collection queries for a stable multi-page view and task watches for changes after a known resource version. Both depend on retained in-memory journals; neither cursor is a durable checkpoint across supervisor restarts.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Defines task filters, task/run queries, domain continuations, pages, and watch events. |
| `solti-core` | Stores current values and reversible histories, reconstructs snapshots, admits watches, and detects expired positions. |
| Adapter or application | Keeps continuation parameters and predicates consistent, consumes streams, and resynchronizes when history expires. |

Runners and Taskvisor produce lifecycle observations that mutate resources, but they do not own collection pagination or watch cursors.

## Task snapshots

`query_tasks(&TaskQuery)` returns `Result<TaskPage<Task>, CollectionError>`.
On success, the page contains:

- Complete task values in name order.
- One opaque collection `resource_version`.
- An optional `TaskContinuation` for the next page.
- `remaining_item_count` for visible items after this page in the same snapshot.

The first page captures a snapshot. A continuation reconstructs that same snapshot from retained changes, including resource values that have since changed or been deleted. A continuation does not start another live list.

`TaskFilter` combines slot, phases, and labels. These categories are ANDed; multiple phases are ORed. An empty phase set matches every phase. Label-selector requirements are ANDed.

`with_active()` selects `Pending` and `Running`. `with_terminal()` selects terminal phase values. These are resource-phase filters, not queries for physical runtime ownership or idle slots.

Keep the filter unchanged across a continuation chain. Core rejects a different filter with `ContinuationFilterMismatch`. `query_tasks_where` applies its extra predicate before pagination; that predicate must also stay stable across the chain.

## Page limits

Task and TaskRun queries default to 100 items and cap the requested count at 1,000. A count of zero selects the default; it does not request an empty page.

Both query types also have a default 4 MiB compact-JSON item budget. It counts serialized items and commas between them, not the surrounding document or transport framing. `with_item_byte_limit` can lower this target.

The first complete item is returned even if it exceeds the item-byte target. It is returned alone to allow progress and native transport measurement. The target is therefore not a hard encoded-response cap. An adapter must account for its own envelope and encoding separately.

Continuation values are domain objects. A transport can encode them as opaque tokens; callers should return the supplied continuation rather than reconstructing a cursor from visible item fields.

## Run-history snapshots

`query_task_runs(&name, &TaskRunQuery)` returns `Result<Option<TaskRunPage>, CollectionError>`:

- A first-page query returns `None` when the current resource is absent.
- A retained resource with no retained runs returns an empty page, not `None`.
- Runs are ordered by generation and then attempt.
- The page records the resource name and exact UID as well as its run-history version.

A run continuation fixes the original UID. It can still reconstruct that incarnation's retained snapshot after deletion or same-name recreation, provided the run journal position remains available. It must not silently switch to the replacement resource.

TaskRun versions belong to a separate journal from Task collection versions. Do not use a run-page version to start a Task watch.

`query_task_runs_where` accepts one predicate, `Fn(&WorkloadTypeMeta) -> bool`.
It checks the current Task's workload on the first page and each historical
run's captured workload before pagination. Continuations use the original UID
snapshot without checking a replacement current task. Keep that predicate
stable across the continuation chain.

## Start a task watch

`watch_tasks(&filter, resource_version)` returns `Result<TaskWatchSubscription, CollectionError>`. An admitted subscription is a stream of `Result<TaskWatchEvent, CollectionError>`.

| Resume argument | Start behavior |
| --- | --- |
| `None` or `Some("0")` | Emit the current matching snapshot as name-sorted `Added` events, then follow later changes. |
| An exact Task collection version | Replay retained changes strictly after that version, then follow live changes. |

Subscription setup captures the snapshot or replay position together with the live revision receiver. It does not leave a subscribe-after-read gap.

Initial snapshot objects retain their own resource versions. The API does not emit a snapshot-complete bookmark. Do not treat the last initial object's version as the global collection snapshot version.

For a list-then-watch process, read the complete paginated snapshot and retain its page `resource_version`. Start the watch from that version. If it has already expired, discard that incomplete handoff and obtain another snapshot. The [collections example](../crates/solti-core/examples/collections.rs) demonstrates this versioned handoff.

## Events describe collection visibility

Classification compares the previous and current resource against the filter and any `watch_tasks_where` predicate:

| Previous value visible | Current value visible | Event |
| --- | --- | --- |
| No | Yes | `Added` with the current value. |
| Yes | Yes | `Modified` with the current value. |
| Yes | No | `Deleted` with the previous visible value and the change's resource version. |
| No | No | No event for this subscription. |

A `Deleted` event can mean that labels or phase left the filtered collection. It does not necessarily mean the resource was deleted from core.

For exact replay and live changes, the event's object version identifies the delivered change. A resumed stream can therefore continue after the last processed change while that position is retained.

## Retention, admission, and expiry

`StateConfig` separates these budgets:

| Budget | Default | What it retains |
| --- | --- | --- |
| `watch_history_capacity` | 4,096 changes | Shared Task change journal for task snapshots and watches. |
| `watch_history_byte_budget` | 64 MiB | Serialized Task payload in that journal. |
| `run_history_capacity` | 4,096 mutation batches | Reversible TaskRun journal for run-page continuations. |
| `run_history_byte_budget` | 64 MiB | Serialized payload in the run journal. |
| `max_concurrent_task_watches` | 256 | Admitted Task subscriptions. |
| `max_task_watch_initial_replay_bytes` | 64 MiB | Aggregate compact Task JSON in initial and exact-replay buffers. |

Initial/replay bytes remain charged until each event is yielded or the subscription releases them. Live watchers do not retain a private payload queue: a coalesced revision notification drives lazy reads from the shared journal.

A slow live watcher can lose its required journal position. It then yields `ResourceVersionExpired` once and ends. This is different from output `Lagged`, which reports a gap and continues.

Count pressure and byte pressure can compact histories. A single Task change too large for the journal budget can expire existing watch positions. An oversized run mutation batch still updates current run state but cannot be retained for reverse reconstruction.

| Error | Meaning |
| --- | --- |
| `InvalidResourceVersion` | Malformed version or a revision ahead of this store. |
| `ResourceVersionExpired` | Foreign store/history identity or a compacted position. Start from a new snapshot. |
| `ContinuationFilterMismatch` | Task filters differ from the original cursor. |
| Cursor-not-found or TaskRun task-mismatch variants | Cursor identity does not belong to the requested visible snapshot. |
| `ConcurrentTaskWatchLimitReached` | No watch-count admission is available. |
| `TaskWatchInitialReplayByteLimitExceeded` | Initial/replay payload would exceed the aggregate budget. |

Task watches close when supervisor shutdown begins, before the runtime drain. Stream closure is not proof that all running tasks have completed. Dropping an unused subscription releases its admission ownership.

## Examples and source

- [Collections example](../crates/solti-core/examples/collections.rs): paginate through concurrent mutation and observe filter-entry and filter-exit events.
- [Query model example](../crates/solti-model/examples/task_query.rs): construct filters and continuations.
- [Task query contracts](../crates/solti-model/src/domain/query/task.rs), [run query contracts](../crates/solti-model/src/domain/query/run.rs), [state implementation](../crates/solti-core/src/state/mod.rs), and [retention configuration](../crates/solti-core/src/config.rs).

## See also

- [Output and history](output-and-history.md) for retained attempt semantics.
- [Serving an API](serving-api.md) for adapter boundaries.
- [Task resources](task-resources.md) for identity and resource versions.
