---
title: Configuration and capacity
description: Configure each SDK owner separately and distinguish desired-state, build, execution, retention, output, and transport limits.
---

# Configuration and capacity

There is no single SDK capacity or timeout. A manifest, a routed build, a
retained Task, a Taskvisor-owned value, and an output buffer consume different
resources. Configure the owner of the resource you intend to bound.

The defaults below describe this source version. They are policy defaults,
not benchmark-derived sizing recommendations. `MiB` means 1,048,576 bytes.

## Choose the configuration owner

| Owner | Configuration point | Controls |
|---|---|---|
| Application dependency graph | Cargo features | Which namespaces and implementations exist; not which services are running. |
| One desired Task | Model `TaskSpec` | Slot, workload, attempt timeout, restart/backoff, failure retries, admission, runner selector. |
| Core resource store | `StateConfig` | Retained Tasks/runs, query journals, watch admission, retention sweep. |
| Routed construction | `ReconciliationConfig` | Outer build concurrency, per-runner concurrency, one build deadline. |
| Attempt runtime | Taskvisor `SupervisorConfig` | Attempt concurrency, registry/ownership bounds, event ingress, stop grace. |
| Slot controller | Taskvisor `ControllerConfig` | Command intake, slot queues, registry handoff, management capacity. |
| Live task output | `OutputConfig` | Per-task event/payload bounds, chunk truncation, shared payload budget. |
| Persistence dispatch | `PersistenceConfig` plus optional sinks | State delivery admission and best-effort output callback capacity. |
| Execution backend | Exec configuration or custom runner | Platform authority, host controls, container engine and cleanup behavior. |
| Network and operations | API, discovery, TLS, logger and exporter configuration | Independent connection, authentication, heartbeat, scrape, and service policies. |

Pass core/runtime/controller/output/persistence settings to
`SupervisorApiBuilder` before `start`. Features and builder calls do not
configure an application-owned HTTP listener or external database.

## Set per-Task policy deliberately

`TaskSpec::builder(slot, workload, timeout_ms)` requires a positive timeout in
milliseconds. The builder's optional defaults are:

| Field | Default | Meaning |
|---|---|---|
| `restart` | `Never` | No automatic next attempt. |
| `admission` | `DropIfRunning` | A busy-slot submission can be rejected after the resource commit. |
| `max_retries` | `None` | No failure-retry cap; the restart policy still determines whether a retry is eligible. |
| `backoff` | Full jitter, 1,000 ms initial, 30,000 ms maximum, factor 2 | Delay policy for eligible failure retries. |
| `runner_selector` | `None` | No label restriction beyond the workload kind. |

These are **SDK model** defaults, not Taskvisor's standalone `TaskDefaults`.
Read [lifecycle and admission](lifecycle-and-admission.md) before choosing a
policy for a one-shot job, periodic task, or long-running service.

## Bound resource storage and observation

`StateConfig` defaults:

| Setting | Default | Counted resource or behavior |
|---|---|---|
| `max_retained_tasks` | 1,024 | All stored Tasks, including Embedded, pending, running, and terminal resources. |
| `max_retained_task_manifest_bytes` | 256 MiB | Canonical compact JSON for caller-owned manifests; excludes status and runs. |
| `max_retained_task_run_bytes` | 256 MiB | Current retained TaskRun values, independent of the run journal. |
| `max_runs_per_task` | 256 | Completed runs per Task; active runs are not removed by this count limit. |
| `run_ttl` | 1 hour | Age of eligible finished or unbound nonterminal runs. |
| `task_ttl` | 1 hour | Age since terminal transition; deletion also requires no runtime binding and empty run history. |
| `sweep_interval` | 5 minutes | Retention pass cadence, not an exact expiration deadline. |
| `watch_history_capacity` | 4,096 | Retained Task changes used by Task snapshots and watches. |
| `watch_history_byte_budget` | 64 MiB | Compact JSON budget for that Task journal. |
| `run_history_capacity` | 4,096 | Reversible TaskRun mutation batches, not the number of current runs. |
| `run_history_byte_budget` | 64 MiB | Compact JSON budget for the TaskRun journal. |
| `max_concurrent_task_watches` | 256 | Admitted subscriptions. |
| `max_task_watch_initial_replay_bytes` | 64 MiB | Aggregate Task JSON retained in initial/replay buffers. |

Task count saturation rejects a new name atomically; it does not evict another
Task. Existing-resource writes remain subject to other limits. Manifest growth
can fail while a shrinking or unchanged manifest fits.

Run-byte pressure compacts the oldest completed runs. If active values alone
cannot fit, core can omit a new active run from retained query state while
keeping the lifecycle handle needed for terminal projection. Execution does not
stop merely because history cannot retain that value.

Journal compaction invalidates old continuation/watch positions. It is not
controlled by Task/run TTL alone. Watch live delivery uses the shared journal;
payload already yielded to a caller is outside the initial/replay budget.
See [collections and watches](collections-and-watches.md) and
[output and history](output-and-history.md).

Optional limits expose `None` to disable that specific bound. Other limits
remain active. `max_runs_per_task = 0` removes every completed run but keeps
active runs. Zero is not a general spelling for unlimited capacity.

## Bound construction separately from execution

| `ReconciliationConfig` setting | Default | Scope |
|---|---|---|
| `build_timeout` | 30 seconds | Starts before outer admission and includes nested catalog builds and admission waits. |
| `max_concurrent_builds` | 32 | Outer routed builds; nested construction shares the outer slot. |
| `max_concurrent_builds_per_runner` | 8 | Builds using one registered runner, including nested catalog selection. |

Embedded work already carries a TaskRef and bypasses these build slots.
Increasing build concurrency does not increase Taskvisor attempt concurrency
or release a busy execution slot. See [reconciliation](reconciliation.md).

## Keep runtime and controller bounds distinct

The workspace selects Taskvisor 0.9.0. Core forwards these configurations;
their defaults come from that dependency:

| `SupervisorConfig` setting | Default | Scope |
|---|---|---|
| `max_concurrent` | `None` | Simultaneously running attempts; a started attempt holds capacity until its physical attempt boundary exits. |
| `max_registered_tasks` | 1,024 | Registry and cleanup admission accounting. |
| `ownership_capacity` | 1,024 | Accepted task values and configured subscribers through retained lifetime and isolated destruction. |
| `bus_capacity` | 1,024 | Event ingress; a finite queue does not make events reliable. |
| `registry_queue_capacity` | 1,024 | Registry command intake. |
| `grace` | 60 seconds | Cooperative stop window before logical force-abort. |
| `subscriber_shutdown_timeout` | 5 seconds | Subscriber queue drain; cannot interrupt a callback already running. |

Core installs one Taskvisor subscriber for its observer. Each external
subscriber consumes another ownership unit. A terminal outcome does not prove
that ownership or attempt capacity has been released.

`ControllerConfig` defaults are 1,024 for `queue_capacity`,
`admission_capacity`, `identity_operation_capacity`, `max_controller_slots`,
and `max_total_pending`. `max_slot_queue` defaults to 100 queued items behind
one busy slot owner. These limits protect different handoffs; raising one does
not raise the others. A zero per-slot queue rejects queued work behind a busy
owner, but does not disable replacement admission.

See [lifecycle and admission](lifecycle-and-admission.md) for acknowledgements
and [cancellation and shutdown](cancellation-and-shutdown.md) for physical
ownership. The dependency's [configuration guide](https://github.com/soltiHQ/taskvisor/blob/v0.9.0/docs/configuration.md)
describes its standalone controls.

## Budget output and external delivery separately

`OutputConfig` defaults are 256 events per Task, a 64 KiB maximum chunk,
16 MiB per-task payload budget, and 256 MiB aggregate core-owned payload.
The effective ring capacity is the power of two rounded down from the stricter
event and byte-derived count bounds.

Empty channels reserve no payload. Retained bytes are charged at their actual
length; subscribers share the core-owned charge. Aggregate exhaustion drops
new payload and is reported as lag. Oversized chunks become exact prefixes
marked as truncated. These limits do not include payload already owned by a
caller or external sink copy.

`PersistenceConfig` defaults:

| Setting | Default | Admission boundary |
|---|---|---|
| `state_queue_capacity` | 2,048 | `reserved + buffered + active <= capacity + 1`; at most one active callback. |
| `state_queue_byte_capacity` | 256 MiB | Conservative pre-write reservations, buffered events, and the active callback. |
| `output_queue_capacity` | 2,048 | Buffered output events plus the active callback. |

State delivery waits for admission before the global state lock. Output
publication never waits for external callback capacity. Both have separate
health/status APIs. There is no dispatcher when its sink is not installed.
See [persistence](persistence.md).

The state queue must admit an atomic commit: its count minimum is 2 and its
current byte minimum is 16 MiB plus 392 KiB. Logical JSON/payload budgets are
not allocator or process RSS limits. Application copies, task values, network
buffers, and backend resources need their own accounting.

## Apply checked configuration

This helper uses example values, not a recommended production profile:

```rust
use std::time::Duration;
use solti::{
    core::{ConfigError, OutputConfig, ReconciliationConfig, StateConfig, SupervisorApi},
    runner::RunnerRouter,
};

fn configured(
    router: RunnerRouter,
) -> Result<solti::core::SupervisorApiBuilder, ConfigError> {
    let state = StateConfig::new()
        .try_with_max_retained_tasks(512)?
        .try_with_max_concurrent_task_watches(64)?
        .try_with_sweep_interval(Duration::from_secs(30))?;
    let builds = ReconciliationConfig::new()
        .try_with_max_concurrent_builds(16)?
        .try_with_max_concurrent_builds_per_runner(4)?;
    let output = OutputConfig::try_new(128)?
        .try_with_byte_limits(8 * 1024 * 1024, 64 * 1024)?;

    Ok(SupervisorApi::builder(router)
        .with_state_config(state)
        .with_reconciliation_config(builds)
        .with_output_config(output))
}
```

Use checked setters for untrusted configuration and handle non-exhaustive
errors with a fallback arm. Core forward deadlines such as sweep interval and
build timeout must be positive and cannot exceed 30 years; elapsed-age TTLs
have a different contract. `start` can still reject invalid Taskvisor
configuration or fail to start an owned worker.

## Follow the boundary-specific settings

- [Subprocesses](subprocesses.md) and [containers and isolation](containers-and-isolation.md): host controls, execution limits, engine ownership.
- [Task API](serving-api.md): transport request/response budgets and authentication policy.
- [Discovery](discovery.md): heartbeat interval, connect/request deadlines, holds and backoff.
- [TLS and authentication](tls-and-authentication.md): trust material and access decisions.
- [Observability](observability.md): logging, scrape capacity/bytes/deadlines and metric injection.
- [Production boundaries](production-boundaries.md): compose these limits into an application contract.

Source: [core configuration](../crates/solti-core/src/config.rs),
[output configuration](../crates/solti-core/src/output.rs),
[persistence configuration](../crates/solti-core/src/persistence.rs),
[builder wiring](../crates/solti-core/src/supervisor/builder.rs),
[TaskSpec defaults](../crates/solti-model/src/resource/spec.rs),
[positive attempt timeout](../crates/solti-model/src/domain/timeout.rs), and
[Taskvisor dependency pin](../Cargo.toml).
