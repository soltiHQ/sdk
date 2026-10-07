---
title: Calendar scheduling
description: Run existing async work on an explicit IANA timezone calendar while retaining Taskvisor lifetime ownership.
---

# Calendar scheduling

`solti-cron` owns calendar evaluation and sequential invocation of an existing `TaskRef`.
Taskvisor owns the outer registration, retry policy, cancellation grace, and shutdown.
Core can supervise the wrapper through the existing Embedded resource path.
The calendar adds no workload GVK or wire protocol.

## Construct a calendar task

Enable `solti` features `cron` and `core` when using Embedded resources:

```rust,no_run
use std::time::Duration;
use solti::{
    core::SupervisorApi,
    cron::{CronSchedule, CronTask},
    model::{EmbeddedSpec, RestartPolicy, TaskManifest, TaskSpec, TaskWorkload},
    runner::RunnerRouter,
    taskvisor::TaskFn,
};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let schedule = CronSchedule::parse("0 9 * * MON-FRI", "Asia/Tbilisi")?;
let job = TaskFn::arc(|ctx| async move {
    ctx.run_until_cancelled(async { /* application maintenance */ }).await?;
    Ok(())
});
let task = CronTask::new(schedule, job, Duration::from_secs(30))?.into_task();
let spec = TaskSpec::builder(
    "maintenance",
    TaskWorkload::Embedded(EmbeddedSpec::new("weekday-maintenance-v1")?),
    7 * 24 * 60 * 60 * 1_000_u64,
).restart(RestartPolicy::Never).build()?;
let manifest = TaskManifest::new("weekday-maintenance", spec)?;
let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;
api.create_embedded_task(manifest, task).await?;
// The application owns the seven-day outer lifetime and shutdown timing.
api.shutdown().await?;
# Ok(())
# }
```

The 30-second invocation timeout bounds each job.
The seven-day outer timeout bounds the complete wrapper attempt, including calendar waiting.
Choose that lifetime and its restart policy for the application.
Direct Taskvisor registration permits `TaskSpec::with_timeout(None)` for an indefinite wrapper lifetime.
Each invocation shares the outer attempt's status and history.
Use [task_cron](../crates/solti/examples/task_cron.rs) for a bounded runnable demonstration with real invocations and cancellation.

## Calendar and concurrency semantics

Five fields use minute, hour, day of month, month, and day of week.
Six fields add seconds first.
The IANA timezone is always explicit; `next_after` returns strictly future UTC instants.
Day of month and weekday restrictions use cron's OR semantics.
Lists, ranges, steps, names, `L`, `W`, and `#` follow the pinned Croner parser.
Year fields and `@daily` style nicknames are rejected.

Fixed local times in a DST gap run at the first valid instant after the gap when it remains on the scheduled local date, including same-date gaps longer than two hours.
Gaps crossing a local date boundary retain Croner's bounded search behavior and may return `NoOccurrence`.
Fixed local times in a repeated hour use the first occurrence.
Wildcard/interval schedules skip missing local times and can run in both passes through the repeated hour.
The bundled chrono-tz database supplies timezone rules.

One wrapper attempt runs one invocation at a time.
Occurrences during active work are skipped.
The currently awaited occurrence may run late within a positive tolerance, 60 seconds by default.
`with_max_lateness` changes the tolerance.
Stale occurrences are skipped without a backlog replay.
Waiting rechecks wall time at least once per second.
A backward adjustment cannot repeat a UTC occurrence within one attempt.
A fresh retry or process restart starts from current time.
Separate registrations of the same wrapper can invoke its job concurrently.

## Cancellation and failure

Each invocation receives a child `TaskContext`.
Parent cancellation signals that context and allows its cooperative asynchronous cleanup to finish within the existing invocation deadline.
Taskvisor's outer shutdown grace can abort the wrapper earlier.
Invocation timeout signals the child before dropping the invocation and reports `TaskError::Timeout`.
Job failures propagate unchanged to Taskvisor's outer retry policy.
Calendar search failures are fatal.
No invocation future or timer is detached from the wrapper.

Job futures must be safe to drop and their destructors must be non-blocking.
The crate does not persist schedules, cursor state, or invocation history, and does not provide cross-process exclusion or exactly-once side effects.
See [production boundaries](production-boundaries.md) for application-owned durability and idempotency.
