//! # Task lifecycle phases.
//!
//! [`TaskPhase`] represents the current state of a task in the supervision lifecycle.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{ModelError, ModelResult};

/// Current execution phase of a single task attempt.
///
/// Phases describe the state of the **current attempt**.
///
/// ## Also
///
/// - [`TaskStatus`](crate::TaskStatus) carries the current phase.
/// - [`TaskPhase::is_terminal`] checks for final states.
/// - [`RestartPolicy`](crate::RestartPolicy) governs what happens after a terminal phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TaskPhase {
    /// Task is queued or waiting to start.
    Pending,
    /// Task is currently executing.
    Running,
    /// Task completed successfully.
    Succeeded,
    /// Attempt failed with an error.
    Failed,
    /// Task exceeded its timeout limit.
    Timeout,
    /// Task was explicitly canceled.
    Canceled,
    /// Task exhausted its restart budget and will not retry.
    Exhausted,
}

impl fmt::Display for TaskPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskPhase::Pending => f.write_str("pending"),
            TaskPhase::Running => f.write_str("running"),
            TaskPhase::Succeeded => f.write_str("succeeded"),
            TaskPhase::Failed => f.write_str("failed"),
            TaskPhase::Timeout => f.write_str("timeout"),
            TaskPhase::Canceled => f.write_str("canceled"),
            TaskPhase::Exhausted => f.write_str("exhausted"),
        }
    }
}

impl FromStr for TaskPhase {
    type Err = ModelError;

    /// Parse a phase name (case-insensitive, trimmed). Accepts the same
    /// camelCase form produced by [`fmt::Display`] / serde.
    fn from_str(s: &str) -> ModelResult<Self> {
        let trimmed = s.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "pending" => Ok(TaskPhase::Pending),
            "running" => Ok(TaskPhase::Running),
            "succeeded" => Ok(TaskPhase::Succeeded),
            "failed" => Ok(TaskPhase::Failed),
            "timeout" => Ok(TaskPhase::Timeout),
            "canceled" => Ok(TaskPhase::Canceled),
            "exhausted" => Ok(TaskPhase::Exhausted),
            _ => Err(ModelError::UnknownTaskPhase(trimmed.to_string())),
        }
    }
}

impl TaskPhase {
    /// Returns `true` if the current attempt has reached a final state.
    ///
    /// A terminal phase means this attempt will not transition further.
    /// The supervisor may still start a **new** attempt based on the [`RestartPolicy`](crate::RestartPolicy).
    #[inline]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskPhase::Succeeded
                | TaskPhase::Failed
                | TaskPhase::Timeout
                | TaskPhase::Canceled
                | TaskPhase::Exhausted
        )
    }

    /// Returns `true` if the task is still active (pending or running).
    #[inline]
    pub fn is_active(&self) -> bool {
        matches!(self, TaskPhase::Pending | TaskPhase::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states() {
        assert!(TaskPhase::Succeeded.is_terminal());
        assert!(TaskPhase::Failed.is_terminal());
        assert!(TaskPhase::Timeout.is_terminal());
        assert!(TaskPhase::Canceled.is_terminal());
        assert!(TaskPhase::Exhausted.is_terminal());

        assert!(!TaskPhase::Pending.is_terminal());
        assert!(!TaskPhase::Running.is_terminal());
    }

    #[test]
    fn active_states() {
        assert!(TaskPhase::Pending.is_active());
        assert!(TaskPhase::Running.is_active());

        assert!(!TaskPhase::Succeeded.is_active());
        assert!(!TaskPhase::Failed.is_active());
    }

    #[test]
    fn serde_roundtrip() {
        let status = TaskPhase::Running;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""running""#);

        let back: TaskPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }

    #[test]
    fn from_str_all_variants() {
        let cases = [
            ("pending", TaskPhase::Pending),
            ("running", TaskPhase::Running),
            ("succeeded", TaskPhase::Succeeded),
            ("failed", TaskPhase::Failed),
            ("timeout", TaskPhase::Timeout),
            ("canceled", TaskPhase::Canceled),
            ("exhausted", TaskPhase::Exhausted),
        ];
        for (s, expected) in cases {
            assert_eq!(s.parse::<TaskPhase>().unwrap(), expected);
        }
    }

    #[test]
    fn from_str_is_case_insensitive_and_trims() {
        assert_eq!("RUNNING".parse::<TaskPhase>().unwrap(), TaskPhase::Running);
        assert_eq!(
            "  Succeeded  ".parse::<TaskPhase>().unwrap(),
            TaskPhase::Succeeded
        );
    }

    #[test]
    fn from_str_roundtrips_display() {
        for phase in [
            TaskPhase::Pending,
            TaskPhase::Running,
            TaskPhase::Succeeded,
            TaskPhase::Failed,
            TaskPhase::Timeout,
            TaskPhase::Canceled,
            TaskPhase::Exhausted,
        ] {
            let rendered = phase.to_string();
            assert_eq!(rendered.parse::<TaskPhase>().unwrap(), phase);
        }
    }

    #[test]
    fn from_str_unknown_errors() {
        let err = "bogus".parse::<TaskPhase>().unwrap_err();
        assert!(matches!(err, ModelError::UnknownTaskPhase(_)));
    }
}
