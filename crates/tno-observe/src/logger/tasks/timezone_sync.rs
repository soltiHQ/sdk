#![cfg(feature = "timezone-sync")]
//! Timezone synchronization task spec for the logger.
//!
//! Periodically syncs the local timezone offset to detect DST transitions
//! without requiring process restart.
//!
//! Backoff policy (current implementation):
//! - On success: fixed delay of 1 hour before next sync.
//! - On failure: retries with fixed 60s delay (no growth yet).
//!
//! Requires:
//! - `init_local_offset()` called in `main()` before tokio runtime.
//! - `timezone-sync` feature flag.
use taskvisor::{TaskError, TaskFn, TaskRef};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::logger::object::timezone::sync_local_offset;
use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy, TaskKind,
};

/// Timeout applied to each sync operation (in milliseconds).
pub const TZ_SYNC_TIMEOUT_MS: u64 = 60_000;

/// Delay after a successful sync (in milliseconds).
pub const TZ_SYNC_RETRY_MS: u64 = 3_600_000;

/// Name of the internal timezone-sync task.
pub const TZ_SYNC_TASK_NAME: &str = "tno-logger-tz-sync";

/// Build the timezone sync task and its corresponding `CreateSpec`.
///
/// Returns:
/// - `TaskRef`    — concrete task body.
/// - `CreateSpec` — model-level specification with restart/backoff/admission policies.
///
/// # Example
/// ```no_run
/// let (task, spec) = timezone_sync_spec();
/// api.submit_with_task(task, &spec).await?;
/// ```
pub fn timezone_sync() -> (TaskRef, CreateSpec) {
    let task: TaskRef = TaskFn::arc(TZ_SYNC_TASK_NAME, |ctx: CancellationToken| async move {
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
        first_ms: TZ_SYNC_RETRY_MS,
        max_ms: TZ_SYNC_RETRY_MS,
        factor: 1.0,
    };
    let spec = CreateSpec {
        restart: RestartStrategy::periodic(TZ_SYNC_RETRY_MS),
        admission: AdmissionStrategy::Replace,
        slot: TZ_SYNC_TASK_NAME.to_string(),
        timeout_ms: TZ_SYNC_TIMEOUT_MS,
        kind: TaskKind::Fn,
        backoff,
    };
    (task, spec)
}
