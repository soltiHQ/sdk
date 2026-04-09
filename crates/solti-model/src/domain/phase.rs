//! # Task lifecycle phases.
//!
//! [`TaskPhase`] represents the current state of a task in the supervision lifecycle.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Current execution phase of a single task attempt.
///
/// Phases describe the state of the **current attempt**.
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
}
