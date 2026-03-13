use solti_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy, RunnerLabels,
    TaskKind,
};
use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::logger::object::timezone::sync_local_offset;

/// Logical slot name for the timezone sync task.
pub const TZ_SYNC_SLOT: &str = "solti-logger-tz-sync";

/// Per-attempt timeout in milliseconds (60 seconds).
pub const TZ_SYNC_TIMEOUT_MS: u64 = 60_000;

/// Interval between successful sync attempts in milliseconds (1 hour).
pub const TZ_SYNC_PERIOD_MS: u64 = 3_600_000;

/// Initial backoff delay on failure (ms).
const BACKOFF_FIRST_MS: u64 = 5_000;

/// Maximum backoff delay on repeated failures (ms).
const BACKOFF_MAX_MS: u64 = 300_000;

/// Backoff multiplier per consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

/// Builds the timezone sync task and its supervision specification.
///
/// The task re-detects the local UTC offset by calling `UtcOffset::current_local_offset()` and updating the global cache.
/// This keeps log timestamps correct across DST transitions in long-running daemons.
///
/// ## Scheduling
///
/// | Scenario      | Delay           | Strategy                              |
/// |---------------|-----------------|---------------------------------------|
/// | Success       | 1 hour          | Periodic restart                      |
/// | Failure       | 5 s → 5 min     | Exponential backoff with equal jitter |
/// | Duplicate     | Replaces        | [`AdmissionStrategy::Replace`]        |
///
/// ## Example
///
/// ```rust,ignore
/// use solti_observe::timezone_sync;
/// use solti_core::TaskPolicy;
///
/// let (task, spec) = timezone_sync();
/// let policy = TaskPolicy::from_spec(&spec);
/// supervisor.submit_with_task(task, &policy).await?;
/// ```
pub fn timezone_sync() -> (TaskRef, CreateSpec) {
    let task: TaskRef = TaskFn::arc(TZ_SYNC_SLOT, |ctx: CancellationToken| async move {
        debug!("timezone sync started");

        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }
        match sync_local_offset() {
            Ok(()) => {
                debug!("timezone offset sync success");
                Ok(())
            }
            Err(e) => Err(TaskError::Fail {
                reason: format!("failed to sync timezone offset: {e}"),
            }),
        }
    });

    let backoff = BackoffStrategy {
        jitter: JitterStrategy::Equal,
        first_ms: BACKOFF_FIRST_MS,
        max_ms: BACKOFF_MAX_MS,
        factor: BACKOFF_FACTOR,
    };

    let spec = CreateSpec {
        restart: RestartStrategy::periodic(TZ_SYNC_PERIOD_MS),
        slot: TZ_SYNC_SLOT.to_string(),
        timeout_ms: TZ_SYNC_TIMEOUT_MS,

        admission: AdmissionStrategy::Replace,
        labels: RunnerLabels::default(),
        kind: TaskKind::None,
        backoff,
    };

    (task, spec)
}
