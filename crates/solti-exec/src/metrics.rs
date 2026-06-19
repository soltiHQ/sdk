//! Metrics helpers for exec runners.
//!
//! ## Overview
//!
//! Provides runner-type identifiers and a mapping from [`TaskError`] to [`MetricOutcome`]
//! used by all runner implementations in this crate.

use solti_runner::MetricOutcome;
use taskvisor::TaskError;

/// Classify a [`TaskError`] into a [`MetricOutcome`] metric label.
///
/// Strips operational details (reason strings, durations) and returns only the category for dashboards:
/// - `Fail` / `Fatal` → [`MetricOutcome::Failure`]
/// - `Canceled` → [`MetricOutcome::Canceled`]
/// - `Timeout`  → [`MetricOutcome::Timeout`]
pub(crate) fn classify_task_error(error: &TaskError) -> MetricOutcome {
    match error {
        TaskError::Fail { .. } | TaskError::Fatal { .. } => MetricOutcome::Failure,
        TaskError::Timeout { .. } => MetricOutcome::Timeout,

        TaskError::Canceled => MetricOutcome::Canceled,
        _ => MetricOutcome::Failure,
    }
}
