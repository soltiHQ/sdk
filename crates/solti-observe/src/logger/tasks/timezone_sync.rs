use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
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
/// | Duplicate     | Replaces        | [`AdmissionPolicy::Replace`]          |
///
/// ## Example
///
/// ```text
/// use solti_observe::timezone_sync;
///
/// let (task, spec) = timezone_sync();
/// supervisor.submit_with_task(task, &spec).await?;
/// ```
pub fn timezone_sync() -> (TaskRef, TaskSpec) {
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
                exit_code: None,
            }),
        }
    });

    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: BACKOFF_FIRST_MS,
        max_ms: BACKOFF_MAX_MS,
        factor: BACKOFF_FACTOR,
    };
    let spec = TaskSpec::builder(TZ_SYNC_SLOT, TaskKind::Embedded, TZ_SYNC_TIMEOUT_MS)
        .restart(RestartPolicy::periodic(TZ_SYNC_PERIOD_MS))
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .build()
        .expect("timezone sync spec must be valid");

    (task, spec)
}
