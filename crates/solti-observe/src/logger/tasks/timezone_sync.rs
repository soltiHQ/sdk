use taskvisor::{TaskError, TaskFn, TaskRef};
use solti_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, JitterStrategy, RestartStrategy, RunnerLabels,
    TaskKind,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::logger::object::timezone::sync_local_offset;

/// Logical slot name used for timezone sync task.
///
/// Ensures that only one sync operation runs at any time.
pub const TZ_SYNC_SLOT: &str = "solti-logger-tz-sync";

/// Per-attempt timeout in milliseconds.
///
/// If syncing the timezone offset takes longer than this limit,
/// the task is considered failed and restart/backoff logic applies.
pub const TZ_SYNC_TIMEOUT_MS: u64 = 60_000;

/// Delay between successful sync attempts in milliseconds.
///
/// This defines the periodic nature of the timezone-sync task.
pub const TZ_SYNC_RETRY_MS: u64 = 3_600_000;

/// Build the timezone sync task and its model-level specification.
///
/// Returns:
/// - [`TaskRef`]    — executable task body.
/// - [`CreateSpec`] — restart/backoff/admission policy and slot binding.
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
        first_ms: TZ_SYNC_TIMEOUT_MS,
        max_ms: TZ_SYNC_TIMEOUT_MS,
        factor: 1.0,
    };
    let spec = CreateSpec {
        slot: TZ_SYNC_SLOT.to_string(),
        timeout_ms: TZ_SYNC_TIMEOUT_MS,
        restart: RestartStrategy::periodic(TZ_SYNC_RETRY_MS),
        backoff,
        admission: AdmissionStrategy::Replace,
        kind: TaskKind::None,
        labels: RunnerLabels::default(),
    };
    (task, spec)
}
