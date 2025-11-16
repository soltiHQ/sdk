#![cfg(feature = "timezone-sync")]

//! Timezone synchronization controller for long-running processes.
//!
//! Periodically syncs the local timezone offset (every 24 hours) to detect DST transitions without requiring process restart.
//!
//! **Backoff policy:**
//! - Success: waits 24 hours before next sync
//! - Failure: exponential backoff starting at 60s, max 1 hour
//!
//! **Requires:**
//! - `init_local_offset()` called in `main()` before tokio runtime
//! - `timezone-sync` feature flag

use std::time::Duration;

use taskvisor::{
    BackoffPolicy, ControllerSpec, JitterPolicy, RestartPolicy::Always, TaskError, TaskFn, TaskRef,
    TaskSpec,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::logger::object::timezone::sync_local_offset;

/// Creates a timezone synchronization controller.
///
/// Returns a `ControllerSpec` that periodically updates the local offset.
/// On success, waits 24 hours. On failure, retries with exponential backoff.
pub fn timezone_sync() -> ControllerSpec {
    const SYNC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

    let task: TaskRef = TaskFn::arc("tno-logger-tz-sync", |ctx: CancellationToken| async move {
        debug!("Timezone sync started");

        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }
        match sync_local_offset() {
            Ok(()) => {
                debug!("Timezone offset sync success");
                Ok(())
            }
            Err(e) => Err(TaskError::Fail {
                reason: format!("failed to sync timezone offset: {}", e),
            }),
        }
    });

    let backoff = BackoffPolicy {
        success_delay: Some(SYNC_INTERVAL),
        jitter: JitterPolicy::None,
        factor: 2.0,

        first: Duration::from_secs(60),
        max: Duration::from_secs(3600),
    };
    ControllerSpec::replace(TaskSpec::new(
        task,
        Always,
        backoff,
        Some(Duration::from_secs(60)),
    ))
}
