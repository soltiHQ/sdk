use serde::{Deserialize, Serialize};

/// Current execution state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    /// Task is queued or waiting to start.
    Pending,
    /// Task is currently executing.
    Running,
    /// Task completed successfully.
    Succeeded,
    /// Task failed with an error.
    Failed,
    /// Task exceeded its timeout limit.
    Timeout,
    /// Task was explicitly canceled.
    Canceled,
    /// Task exhausted its restart policy and will not retry.
    Exhausted,
}

impl TaskStatus {
    /// Returns `true` if the task is in a terminal state (won't transition further).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded
                | TaskStatus::Failed
                | TaskStatus::Timeout
                | TaskStatus::Canceled
                | TaskStatus::Exhausted
        )
    }

    /// Returns `true` if the task is still active (pending or running).
    pub fn is_active(&self) -> bool {
        matches!(self, TaskStatus::Pending | TaskStatus::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states() {
        assert!(TaskStatus::Succeeded.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Timeout.is_terminal());
        assert!(TaskStatus::Canceled.is_terminal());
        assert!(TaskStatus::Exhausted.is_terminal());

        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
    }

    #[test]
    fn active_states() {
        assert!(TaskStatus::Pending.is_active());
        assert!(TaskStatus::Running.is_active());

        assert!(!TaskStatus::Succeeded.is_active());
        assert!(!TaskStatus::Failed.is_active());
    }

    #[test]
    fn serde_roundtrip() {
        let status = TaskStatus::Running;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, r#""running""#);

        let back: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
    }
}
