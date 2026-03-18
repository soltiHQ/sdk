use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::TaskId;

/// Resource metadata.
///
/// `ObjectMeta` pattern:
/// - `generation` increments on spec changes (user-driven mutations)
/// - `resource_version` increments on ANY change (including status transitions)
///
/// Slot and labels live in [`crate::TaskSpec`]: the single source of truth for user-provided scheduling intent.
/// The [`crate::Task`] provides convenience accessors that delegate to spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMeta {
    /// Unique task identifier.
    pub id: TaskId,
    /// Incremented on spec changes only.
    pub generation: u64,
    /// Incremented on any change (spec or status).
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
    pub fn new(id: TaskId) -> Self {
        let now = SystemTime::now();

        Self {
            id,
            generation: 1,
            resource_version: 1,
            created_at: now,
            updated_at: now,
        }
    }

    /// Increment generation (spec changed) and resource_version.
    pub fn bump_generation(&mut self) {
        self.updated_at = SystemTime::now();
        self.resource_version += 1;
        self.generation += 1;
    }

    /// Increment resource_version only (status transition).
    pub fn bump_resource_version(&mut self) {
        self.updated_at = SystemTime::now();
        self.resource_version += 1;
    }
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
        let ms = since_epoch.as_secs() * 1_000 + u64::from(since_epoch.subsec_millis());
        ms.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + std::time::Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_defaults() {
        let meta = ObjectMeta::new("test-id".into());
        assert_eq!(meta.id, "test-id");
        assert_eq!(meta.generation, 1);
        assert_eq!(meta.resource_version, 1);
    }

    #[test]
    fn bump_generation_increments_both() {
        let mut meta = ObjectMeta::new("t".into());
        meta.bump_generation();
        assert_eq!(meta.generation, 2);
        assert_eq!(meta.resource_version, 2);
    }

    #[test]
    fn bump_resource_version_only() {
        let mut meta = ObjectMeta::new("t".into());
        meta.bump_resource_version();
        assert_eq!(meta.generation, 1);
        assert_eq!(meta.resource_version, 2);
    }

    #[test]
    fn serde_roundtrip() {
        let meta = ObjectMeta::new("id-1".into());
        let json = serde_json::to_string(&meta).unwrap();
        let back: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, meta.id);
        assert_eq!(back.generation, 1);
        assert_eq!(back.resource_version, 1);
    }
}
