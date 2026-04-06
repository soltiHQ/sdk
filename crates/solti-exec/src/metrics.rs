//! Metrics helpers for exec runners.
//!
//! ## Overview
//!
//! Provides runner-type identifiers and a mapping from [`TaskError`] to [`TaskOutcome`]
//! used by all runner implementations in this crate.

use solti_runner::TaskOutcome;
use taskvisor::TaskError;

/// Classify a [`TaskError`] into a [`TaskOutcome`] metric label.
///
/// Strips operational details (reason strings, durations) and returns only the category for dashboards:
/// - `Fail` / `Fatal` → [`TaskOutcome::Failure`]
/// - `Canceled` → [`TaskOutcome::Canceled`]
/// - `Timeout`  → [`TaskOutcome::Timeout`]
pub(crate) fn classify_task_error(error: &TaskError) -> TaskOutcome {
    match error {
        TaskError::Fail { .. } | TaskError::Fatal { .. } => TaskOutcome::Failure,
        TaskError::Timeout { .. } => TaskOutcome::Timeout,

        TaskError::Canceled => TaskOutcome::Canceled,
        _ => TaskOutcome::Failure,
    }
}
