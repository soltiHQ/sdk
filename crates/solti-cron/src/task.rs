use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use taskvisor::{BoxTaskFuture, Task, TaskContext, TaskError, TaskRef};

use crate::{CronError, CronSchedule, DateTime, Utc};

const CLOCK_RECHECK: Duration = Duration::from_secs(1);
const DEFAULT_MAX_LATENESS: Duration = Duration::from_secs(60);

/// Calendar wrapper around one reusable Taskvisor task.
///
/// One attempt waits for future calendar instants and invokes `job` sequentially.
/// Occurrences during an invocation are skipped. Waiting rereads wall time at
/// least once per second, handles forward/backward adjustments, and never
/// revisits an elapsed UTC occurrence within that attempt.
///
/// An invocation failure exits the wrapper with the original [`TaskError`].
/// Taskvisor's restart policy then decides whether a fresh wrapper attempt starts.
/// A restart begins from current time and never replays a backlog.
///
/// The invocation deadline cancels its child context before dropping its future.
/// Parent cancellation signals the child and polls its cooperative cleanup until
/// completion or the existing invocation deadline. The outer supervisor's
/// shutdown grace can abort this future earlier. Dropping the wrapper
/// drops its active invocation; no tasks are detached from this future.
/// Invocation futures and their destructors must be safe to drop and non-blocking.
///
/// Each registration/attempt is independent. Registering the same wrapper under
/// multiple names can run its job concurrently; there is no global exclusion.
/// Invocations share the outer Task's status/history rather than creating new
/// Task resources. Configure the outer Taskvisor timeout for the wrapper lifetime,
/// separately from `invocation_timeout`; direct Taskvisor permits `None`.
#[derive(Clone)]
pub struct CronTask {
    schedule: CronSchedule,
    job: TaskRef,
    invocation_timeout: Duration,
    max_lateness: Duration,
}

impl fmt::Debug for CronTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CronTask")
            .field("schedule", &self.schedule)
            .field("invocation_timeout", &self.invocation_timeout)
            .field("max_lateness", &self.max_lateness)
            .finish_non_exhaustive()
    }
}

impl CronTask {
    /// Wraps a task with a calendar and a positive per-invocation deadline.
    ///
    /// The default lateness tolerance is 60 seconds. An occurrence later than
    /// this tolerance is skipped and the next one is computed from current time.
    ///
    /// # Errors
    /// Returns [`CronError::InvalidDuration`] for zero/unrepresentable deadlines.
    pub fn new(
        schedule: CronSchedule,
        job: TaskRef,
        invocation_timeout: Duration,
    ) -> Result<Self, CronError> {
        if invocation_timeout.is_zero() || Instant::now().checked_add(invocation_timeout).is_none()
        {
            return Err(CronError::InvalidDuration("invocation_timeout"));
        }
        Ok(Self {
            schedule,
            job,
            invocation_timeout,
            max_lateness: DEFAULT_MAX_LATENESS,
        })
    }

    /// Sets the positive tolerance for delayed timers or forward clock jumps.
    ///
    /// No backlog is replayed. At most the currently awaited occurrence can run
    /// late, when it remains within this tolerance.
    ///
    /// # Errors
    /// Returns [`CronError::InvalidDuration`] for zero tolerance.
    pub fn with_max_lateness(mut self, max_lateness: Duration) -> Result<Self, CronError> {
        if max_lateness.is_zero() {
            return Err(CronError::InvalidDuration("max_lateness"));
        }
        self.max_lateness = max_lateness;
        Ok(self)
    }

    /// Calendar used by this wrapper.
    #[must_use]
    pub fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    /// Erases this wrapper to the standard Taskvisor executable boundary.
    #[must_use]
    pub fn into_task(self) -> TaskRef {
        Arc::new(self)
    }

    async fn run<C: Clock>(&self, ctx: TaskContext, clock: C) -> Result<(), TaskError> {
        let mut cursor = clock.now();
        loop {
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }
            let due = self
                .schedule
                .next_after(cursor.max(clock.now()))
                .map_err(TaskError::fatal_from)?;
            loop {
                let now = clock.now();
                if now >= due {
                    break;
                }
                let wait = (due - now)
                    .to_std()
                    .unwrap_or(CLOCK_RECHECK)
                    .min(CLOCK_RECHECK);
                ctx.run_until_cancelled(tokio::time::sleep(wait)).await?;
            }
            if ctx.is_cancelled() {
                return Err(TaskError::Canceled);
            }
            let now = clock.now();
            if (now - due).to_std().unwrap_or_default() > self.max_lateness {
                cursor = now;
                continue;
            }
            let child = ctx.child();
            let token = child.cancellation_token();
            let _cancel_on_drop = token.clone().drop_guard();
            let mut invocation = self.job.spawn(child);
            let deadline = tokio::time::sleep(self.invocation_timeout);
            tokio::pin!(deadline);
            let result = tokio::select! {
                biased;
                _ = ctx.cancelled() => {
                    token.cancel();
                    tokio::select! {
                        biased;
                        _ = &mut deadline => {},
                        _ = &mut invocation => {},
                    }
                    Err(TaskError::Canceled)
                },
                _ = &mut deadline => Err(TaskError::timeout(self.invocation_timeout)),
                result = &mut invocation => result,
            };
            token.cancel();
            drop(invocation);
            result?;
            cursor = due.max(clock.now());
        }
    }
}

impl Task for CronTask {
    fn spawn(&self, ctx: TaskContext) -> BoxTaskFuture {
        let task = self.clone();
        Box::pin(async move { task.run(ctx, SystemClock).await })
    }
}

trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        SystemTime::now().into()
    }
}

#[cfg(test)]
mod tests;
