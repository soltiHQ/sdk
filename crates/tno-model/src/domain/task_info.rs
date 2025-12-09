use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::{Slot, TaskId, TaskStatus};

/// Detailed information about a task instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskInfo {
    /// Unique task identifier.
    pub id: TaskId,
    /// Logical slot name.
    pub slot: Slot,
    /// Current execution state.
    pub status: TaskStatus,
    /// Number of execution attempts (starts at 1).
    pub attempt: u32,
    /// When the task was created.
    #[serde(with = "time_serde")]
    pub created_at: SystemTime,
    /// When the task was last updated (state change).
    #[serde(with = "time_serde")]
    pub updated_at: SystemTime,
    /// Last error message (if status is Failed/Timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

mod time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let since_epoch = time
            .duration_since(UNIX_EPOCH)
            .map_err(serde::ser::Error::custom)?;
        since_epoch.as_secs().serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_info_serde_roundtrip() {
        let info = TaskInfo {
            id: TaskId::from("test-task-1"),
            slot: "demo-slot".to_string(),
            status: TaskStatus::Running,
            attempt: 2,
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            error: Some("timeout".to_string()),
        };

        let json = serde_json::to_string(&info).unwrap();
        let back: TaskInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(back.id, info.id);
        assert_eq!(back.slot, info.slot);
        assert_eq!(back.status, info.status);
        assert_eq!(back.attempt, info.attempt);
        assert_eq!(back.error, info.error);
    }

    #[test]
    fn task_info_optional_error() {
        let info = TaskInfo {
            id: TaskId::from("test-task"),
            slot: "slot".to_string(),
            status: TaskStatus::Succeeded,
            attempt: 1,
            created_at: SystemTime::now(),
            updated_at: SystemTime::now(),
            error: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("error"));
    }
}
