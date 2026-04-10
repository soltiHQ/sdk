//! # Task status.
//!
//! [`TaskStatus`] tracks observed state: phase, attempt count, exit code, last error.

use serde::{Deserialize, Serialize};

use crate::TaskPhase;

/// Observed runtime state of a task.
///
/// ## Also
///
/// - [`TaskPhase`] lifecycle phase enum.
/// - [`Task`](crate::Task) aggregate that embeds `TaskStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    /// Current lifecycle phase.
    pub phase: TaskPhase,
    /// Number of execution attempts.
    pub attempt: u32,
    /// Process exit code (Subprocess/Container only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Last error message (present when phase is Failed/Timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TaskStatus {
    /// Create initial pending status.
    pub fn pending() -> Self {
        Self {
            phase: TaskPhase::Pending,
            exit_code: None,
            error: None,
            attempt: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_default() {
        let s = TaskStatus::pending();
        assert_eq!(s.phase, TaskPhase::Pending);
        assert_eq!(s.attempt, 0);
        assert!(s.error.is_none());
    }

    #[test]
    fn error_stored() {
        let s = TaskStatus {
            phase: TaskPhase::Failed,
            attempt: 3,
            exit_code: None,
            error: Some("timeout".into()),
        };
        assert_eq!(s.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn serde_skips_none_error() {
        let s = TaskStatus::pending();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn serde_roundtrip() {
        let s = TaskStatus {
            phase: TaskPhase::Running,
            attempt: 2,
            exit_code: None,
            error: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.phase, TaskPhase::Running);
        assert_eq!(back.attempt, 2);
    }

    #[test]
    fn exit_code_serde() {
        let s = TaskStatus {
            phase: TaskPhase::Failed,
            attempt: 1,
            exit_code: Some(137),
            error: Some("killed".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"exitCode\":137"));

        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.exit_code, Some(137));
    }

    #[test]
    fn serde_skips_none_exit_code() {
        let s = TaskStatus::pending();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("exitCode"));
    }
}
