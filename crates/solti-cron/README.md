# solti-cron

Calendar scheduling for existing Taskvisor tasks.
`CronSchedule` parses a calendar in an explicit IANA timezone.
`CronTask` waits and invokes one reusable `TaskRef` sequentially inside a supervised attempt.
It creates no detached workers or separate child Task resources.

```rust
use std::time::Duration;
use solti_cron::{CronSchedule, CronTask};
use taskvisor::TaskFn;

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let schedule = CronSchedule::parse("0 9 * * MON-FRI", "Asia/Tbilisi")?;
let job = TaskFn::arc(|ctx| async move {
    ctx.run_until_cancelled(async { /* one unit of work */ }).await?;
    Ok(())
});
let task = CronTask::new(schedule, job, Duration::from_secs(30))?.into_task();
// Register `task` in Taskvisor, or submit it as Embedded work through solti-core.
# let _ = task;
# Ok(())
# }
```

## Calendar contract

- Five fields: minute, hour, day of month, month, day of week.
- Six fields: seconds followed by those five fields.
- Lists, ranges, steps, names, `L`, `W`, and `#` use Croner semantics.
- Restricted day of month and weekday fields use OR semantics.
- Weekday 0 or 7 is Sunday.
- Years and aliases such as `@daily` are rejected.
- `next_after` returns a UTC instant strictly after its input.
- Fixed local times in a DST gap run at the first valid instant after the gap when it remains on the scheduled local date. This includes same-date gaps longer than two hours.
- Gaps crossing a local date boundary retain Croner's bounded search behavior and may return `NoOccurrence`.
- Fixed local times in a repeated hour use its first occurrence.
- Wildcard/interval schedules skip missing local times and can use both passes through a repeated hour.
- The bundled chrono-tz database defines timezone rules; updates require a dependency update.

## Execution and ownership

The first invocation happens at the next future occurrence.
One wrapper attempt never overlaps its own invocations.
Occurrences during a running invocation are skipped.
The current awaited occurrence may run up to 60 seconds late by default.
`with_max_lateness` changes this positive tolerance.
Later occurrences are skipped without replaying a backlog.
Wall time is rechecked at least once per second while waiting.
A backward clock adjustment cannot repeat an elapsed UTC occurrence within one attempt.
A restart begins at current wall time with a fresh cursor.

Each invocation receives a child `TaskContext` and its own positive deadline.
Timeout signals child cancellation before dropping the invocation future and returns `TaskError::Timeout`.
Parent cancellation interrupts waiting, signals an active child, and polls its asynchronous cleanup until completion or its existing invocation deadline.
The outer supervisor's shutdown grace can abort the wrapper earlier.
Job errors propagate unchanged; Taskvisor's policy owns retries and backoff.
Calendar search failure is fatal.
Job futures and destructors must be safe to drop and non-blocking.
The wrapper owns every active future and starts no Tokio tasks of its own.

Configure the outer Taskvisor timeout for the wrapper lifetime, separately from the per-invocation timeout.
Direct Taskvisor registration accepts `TaskSpec::with_timeout(None)` for an indefinitely supervised calendar.
Solti core requires a positive outer timeout; choose its lifetime explicitly and its restart policy if expiry should start a fresh calendar attempt.
Do not use the short invocation timeout as the outer timeout for a daily calendar.
Invocation history and status belong to the outer Task attempt.
Registering the same wrapper under several names can run the job concurrently.
Schedules and cursors are in memory and are not persisted or replayed after process restart.
