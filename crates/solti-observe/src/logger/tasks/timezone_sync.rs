//! Timezone-sync supervised task.
//!
//! [`timezone_sync`](crate::timezone_sync) returns a `(TaskRef, TaskSpec)`
//! pair for a periodic task that tries to refresh the local UTC offset.

use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskKind, TaskSpec,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
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

/// Build the timezone sync task and its supervision specification.
///
/// The task calls `UtcOffset::current_local_offset()` and updates the global cache when the platform allows it.
/// On many Unix systems this works best before Tokio starts worker threads, so [`crate::init_local_offset`] is still the important startup call.
///
/// ## Scheduling
///
/// | Scenario      | Delay           | Strategy                              |
/// |---------------|-----------------|---------------------------------------|
/// | Success       | 1 hour          | Periodic restart                      |
/// | Failure       | 5 s to 5 min    | Exponential backoff with equal jitter |
/// | Duplicate     | Replaces        | [`AdmissionPolicy::Replace`]          |
///
/// ## Example
///
/// ```
/// use solti_observe::timezone_sync;
///
/// let (task, spec) = timezone_sync();
///
/// assert_eq!(spec.slot().as_str(), "solti-logger-tz-sync");
/// let _ = task;
/// ```
pub fn timezone_sync() -> (TaskRef, TaskSpec) {
    let task: TaskRef = TaskFn::arc(TZ_SYNC_SLOT, |ctx: TaskContext| async move {
        debug!("timezone sync started");

        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }
        match sync_local_offset() {
            Ok(()) => {
                debug!("timezone offset sync success");
                Ok(())
            }
            Err(e) => Err(TaskError::fail(format!(
                "failed to sync timezone offset: {e}"
            ))),
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
