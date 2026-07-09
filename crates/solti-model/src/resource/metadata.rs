//! Object metadata.
//!
//! [`ObjectMeta`] tracks identity, resource version, and timestamps.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::TaskId;

/// Resource metadata for a task.
///
/// `resource_version` starts at `1` and is bumped when the task changes.
/// Slot and labels live in [`crate::TaskSpec`] as the single source of truth.
///
/// ## Example
///
/// ```
/// use solti_model::{ObjectMeta, TaskId};
///
/// let meta = ObjectMeta::new(TaskId::from("task-1"));
/// assert_eq!(meta.id.as_str(), "task-1");
/// assert_eq!(meta.resource_version, 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMeta {
    /// Unique task identifier.
    pub id: TaskId,
    /// Incremented on any change to the resource (spec or status).
    pub resource_version: u64,
    /// When the resource was created.
    #[serde(with = "time_serde")]
    pub created_at: SystemTime,
    /// When the resource was last updated.
    #[serde(with = "time_serde")]
    pub updated_at: SystemTime,
}

impl ObjectMeta {
    /// Create metadata for a new resource.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ObjectMeta, TaskId};
    ///
    /// let meta = ObjectMeta::new(TaskId::from("task-1"));
    /// assert_eq!(meta.resource_version, 1);
    /// ```
    pub fn new(id: TaskId) -> Self {
        let now = SystemTime::now();

        Self {
            id,
            resource_version: 1,
            created_at: now,
            updated_at: now,
        }
    }

    /// Increment `resource_version` and stamp `updated_at`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::{ObjectMeta, TaskId};
    ///
    /// let mut meta = ObjectMeta::new(TaskId::from("task-1"));
    /// meta.bump_resource_version();
    /// assert_eq!(meta.resource_version, 2);
    /// ```
    pub fn bump_resource_version(&mut self) {
        self.updated_at = SystemTime::now();
        self.resource_version += 1;
    }
}

pub(crate) mod time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        let ms = since_epoch.as_secs() * 1_000 + u64::from(since_epoch.subsec_millis());
        ms.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(ms))
            .ok_or_else(|| <D::Error as serde::de::Error>::custom("timestamp out of range"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_defaults() {
        let meta = ObjectMeta::new("test-id".into());
        assert_eq!(meta.id, "test-id");
        assert_eq!(meta.resource_version, 1);
    }

    #[test]
    fn bump_resource_version_increments() {
        let mut meta = ObjectMeta::new("t".into());
        meta.bump_resource_version();
        assert_eq!(meta.resource_version, 2);
    }

    #[test]
    fn serde_roundtrip() {
        let meta = ObjectMeta::new("id-1".into());
        let json = serde_json::to_string(&meta).unwrap();
        let back: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.resource_version, 1);
    }

    #[test]
    fn serialize_handles_pre_epoch_timestamp() {
        use std::time::{Duration, UNIX_EPOCH};
        let mut meta = ObjectMeta::new("pre-epoch".into());
        meta.created_at = UNIX_EPOCH - Duration::from_secs(10);
        meta.updated_at = UNIX_EPOCH - Duration::from_secs(5);

        let json = serde_json::to_string(&meta).expect("must not fail");
        assert!(json.contains(r#""createdAt":0"#));
        assert!(json.contains(r#""updatedAt":0"#));
    }
}
