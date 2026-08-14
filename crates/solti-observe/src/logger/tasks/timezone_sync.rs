//! # Timezone refresh task
//!
//! [`timezone_sync`] builds a supervised offset refresh task.
//!
//! ```text
//! system local offset ──► refresh attempt  ──► local offset cache
//!                              └── failure ──► retry backoff
//! ```

use solti_model::{
    AdmissionPolicy, BackoffPolicy, EmbeddedSpec, JitterPolicy, RestartPolicy, TaskManifest,
    TaskSpec, TaskWorkload,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tracing::debug;

use crate::logger::object::sync_local_offset;

/// Logical slot for the refresh task.
const TIMEZONE_SYNC_SLOT: &str = "solti-observe-timezone-sync";

/// Per-attempt timeout in milliseconds.
const TIMEZONE_SYNC_TIMEOUT_MS: u64 = 60_000;

/// Interval after a successful attempt in milliseconds.
const TIMEZONE_SYNC_PERIOD_MS: u64 = 3_600_000;

/// Initial failure backoff in milliseconds.
const BACKOFF_FIRST_MS: u64 = 5_000;

/// Maximum failure backoff in milliseconds.
const BACKOFF_MAX_MS: u64 = 300_000;

/// Backoff multiplier after each consecutive failure.
const BACKOFF_FACTOR: f64 = 2.0;

/// Builds the timezone refresh task and its manifest.
///
/// An offset detection failure returns a retryable [`TaskError::Fail`].
/// Cancellation before detection returns [`TaskError::Canceled`].
///
/// ## Refresh
///
/// Each attempt detects the current system offset.
/// A successful attempt replaces the cached value.
/// Later attempts can observe offset changes, including DST transitions.
///
/// ## Task Settings
///
/// | Setting         | Value                                      |
/// |-----------------|--------------------------------------------|
/// | Slot            | `solti-observe-timezone-sync`              |
/// | Workload        | Embedded                                   |
/// | Revision        | `solti-observe@<crate version>`            |
/// | Attempt timeout | 60 seconds                                 |
/// | Success period  | 1 hour                                     |
/// | Backoff base    | 5 seconds to 5 minutes with factor `2`     |
/// | Jitter          | Equal                                      |
/// | Admission       | [`AdmissionPolicy::Replace`]               |
///
/// ## Example
///
/// ```
/// use solti_observe::timezone_sync;
///
/// let (manifest, task_ref) = timezone_sync();
///
/// assert_eq!(manifest.slot().as_str(), "solti-observe-timezone-sync");
/// let _ = task_ref;
/// ```
pub fn timezone_sync() -> (TaskManifest, TaskRef) {
    let task: TaskRef = TaskFn::arc(TIMEZONE_SYNC_SLOT, |ctx: TaskContext| async move {
        debug!(
            event = "logger.timezone_sync",
            stage = "started",
            "timezone sync started"
        );

        if ctx.is_cancelled() {
            return Err(TaskError::Canceled);
        }
        sync_local_offset().map_err(|error| TaskError::fail(error.to_string()))?;
        debug!(
            event = "logger.timezone_sync",
            stage = "completed",
            "timezone sync completed"
        );
        Ok(())
    });

    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: BACKOFF_FIRST_MS,
        max_ms: BACKOFF_MAX_MS,
        factor: BACKOFF_FACTOR,
    };
    let embedded = EmbeddedSpec::new(concat!(
        env!("CARGO_PKG_NAME"),
        "@",
        env!("CARGO_PKG_VERSION")
    ))
    .expect("timezone sync revision must be valid");
    let spec = TaskSpec::builder(
        TIMEZONE_SYNC_SLOT,
        TaskWorkload::Embedded(embedded),
        TIMEZONE_SYNC_TIMEOUT_MS,
    )
    .restart(RestartPolicy::periodic(TIMEZONE_SYNC_PERIOD_MS))
    .backoff(backoff)
    .admission(AdmissionPolicy::Replace)
    .build()
    .expect("timezone sync spec must be valid");

    let manifest = TaskManifest::new(TIMEZONE_SYNC_SLOT, spec)
        .expect("timezone sync Task manifest must be valid");
    (manifest, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_the_refresh_contract() {
        let (manifest, _) = timezone_sync();
        let spec = manifest.spec();

        assert_eq!(manifest.slot().as_str(), TIMEZONE_SYNC_SLOT);
        assert_eq!(spec.timeout().as_millis(), TIMEZONE_SYNC_TIMEOUT_MS);
        assert_eq!(
            spec.restart(),
            RestartPolicy::periodic(TIMEZONE_SYNC_PERIOD_MS)
        );
        assert_eq!(spec.admission(), AdmissionPolicy::Replace);
        assert_eq!(spec.backoff().jitter, JitterPolicy::Equal);
        assert_eq!(spec.backoff().first_ms, BACKOFF_FIRST_MS);
        assert_eq!(spec.backoff().max_ms, BACKOFF_MAX_MS);
        assert_eq!(spec.backoff().factor, BACKOFF_FACTOR);

        let TaskWorkload::Embedded(embedded) = spec.workload() else {
            panic!("timezone sync must use an Embedded workload");
        };
        assert_eq!(
            embedded.revision(),
            concat!(env!("CARGO_PKG_NAME"), "@", env!("CARGO_PKG_VERSION"))
        );
    }
}
