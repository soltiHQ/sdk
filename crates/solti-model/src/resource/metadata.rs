//! # Resource metadata
//!
//! [`ObjectMeta`] contains server-owned identity and version fields.
//! It also contains caller-owned labels and annotations.

use std::{fmt, time::SystemTime};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{Annotations, Labels, ModelError, ModelResult, TaskId};

const UID_ENTROPY_BYTES: usize = 16;

/// Opaque, server-assigned identity of one resource incarnation.
///
/// A UID changes when a resource is deleted and recreated with the same name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::uid"))]
#[serde(transparent)]
pub struct Uid(String);

impl Uid {
    /// Wraps an existing UID.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is empty.
    pub fn new(value: impl Into<String>) -> ModelResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ModelError::Invalid("uid must not be empty".into()));
        }
        Ok(Self(value))
    }

    /// Generates a UID from 128 bits of operating system entropy.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the entropy source is unavailable.
    pub fn generate() -> ModelResult<Self> {
        let mut bytes = [0_u8; UID_ENTROPY_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| {
            ModelError::Invalid(format!("OS entropy source unavailable: {error}").into())
        })?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// Returns the opaque UID value.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Uid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Identity, concurrency version, generation and user metadata for a task.
///
/// `name` is the stable resource address.
/// `uid` identifies one incarnation.
/// `resource_version` is assigned by the state store and remains opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectMeta {
    name: TaskId,
    uid: Uid,
    resource_version: String,
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    generation: u64,
    #[serde(with = "rfc3339_time_serde")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::rfc3339_time")
    )]
    creation_timestamp: SystemTime,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    labels: Labels,
    #[serde(default, skip_serializing_if = "Annotations::is_empty")]
    annotations: Annotations,
}

impl ObjectMeta {
    /// Creates metadata and generates a UID.
    ///
    /// The initial generation is `1`.
    /// The state store assigns the first resource version through [`Self::set_resource_version`].
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the name is invalid or UID generation fails.
    pub fn new(name: TaskId) -> ModelResult<Self> {
        Self::with_uid(name, Uid::generate()?)
    }

    /// Creates metadata with an existing UID.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the name is invalid.
    pub fn with_uid(name: TaskId, uid: Uid) -> ModelResult<Self> {
        name.validate_format()?;
        Ok(Self {
            name,
            uid,
            resource_version: String::new(),
            generation: 1,
            creation_timestamp: time_serde::now(),
            labels: Labels::new(),
            annotations: Annotations::new(),
        })
    }

    /// Stable resource name.
    #[inline]
    pub fn name(&self) -> &TaskId {
        &self.name
    }

    /// Server-assigned identity of this resource incarnation.
    #[inline]
    pub fn uid(&self) -> &Uid {
        &self.uid
    }

    /// Opaque state-store version used for optimistic concurrency.
    #[inline]
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }

    /// Desired-state generation.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Resource creation timestamp.
    #[inline]
    pub fn creation_timestamp(&self) -> SystemTime {
        self.creation_timestamp
    }

    /// Selector metadata.
    #[inline]
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Free-form metadata.
    #[inline]
    pub fn annotations(&self) -> &Annotations {
        &self.annotations
    }

    pub(crate) fn into_manifest_parts(self) -> (TaskId, Labels, Annotations) {
        (self.name, self.labels, self.annotations)
    }

    /// Assigns an opaque state-store version.
    ///
    /// The value is stored verbatim and is never parsed by the model.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is empty.
    pub fn set_resource_version(&mut self, resource_version: impl Into<String>) -> ModelResult<()> {
        let resource_version = resource_version.into();
        if resource_version.trim().is_empty() {
            return Err(ModelError::Invalid(
                "resourceVersion must not be empty when assigned".into(),
            ));
        }
        self.resource_version = resource_version;
        Ok(())
    }

    pub(crate) fn apply_metadata(&mut self, labels: Labels, annotations: Annotations) -> bool {
        if self.labels == labels && self.annotations == annotations {
            return false;
        }
        self.labels = labels;
        self.annotations = annotations;
        true
    }

    pub(crate) fn bump_generation(&mut self) {
        self.generation = self.generation.saturating_add(1);
    }
}

pub(crate) mod time_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub(crate) fn now() -> SystemTime {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        UNIX_EPOCH + std::time::Duration::from_millis(duration.as_millis() as u64)
    }

    pub(crate) fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let since_epoch = time
            .duration_since(UNIX_EPOCH)
            .map_err(serde::ser::Error::custom)?;
        let ms = since_epoch.as_secs() * 1_000 + u64::from(since_epoch.subsec_millis());
        ms.serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(ms))
            .ok_or_else(|| <D::Error as serde::de::Error>::custom("timestamp out of range"))
    }
}

pub(crate) mod rfc3339_time_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::SystemTime;
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};

    pub(crate) fn serialize<S>(timestamp: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        OffsetDateTime::from(*timestamp)
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)
            .and_then(|value| serializer.serialize_str(&value))
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let timestamp =
            OffsetDateTime::parse(&value, &Rfc3339).map_err(serde::de::Error::custom)?;
        Ok(SystemTime::from(timestamp))
    }

    pub(crate) mod option {
        use serde::{Deserialize, Deserializer, Serializer};
        use std::time::SystemTime;

        pub(crate) fn serialize<S>(
            timestamp: &Option<SystemTime>,
            serializer: S,
        ) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match timestamp {
                Some(timestamp) => super::serialize(timestamp, serializer),
                None => serializer.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<SystemTime>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer)?
                .map(|value| {
                    time::OffsetDateTime::parse(
                        &value,
                        &time::format_description::well_known::Rfc3339,
                    )
                    .map(SystemTime::from)
                    .map_err(serde::de::Error::custom)
                })
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_server_identity_and_generation() {
        let meta = ObjectMeta::new(TaskId::new("task-a").unwrap()).unwrap();

        assert_eq!(meta.name(), "task-a");
        assert!(!meta.uid().as_str().is_empty());
        assert_eq!(meta.generation(), 1);
        assert_eq!(meta.resource_version(), "");
    }

    #[test]
    fn server_metadata_uses_kubernetes_fields_and_opaque_resource_version() {
        let mut meta = ObjectMeta::new(TaskId::new("task-a").unwrap()).unwrap();
        meta.set_resource_version("store/revision:0021").unwrap();

        assert_eq!(meta.resource_version(), "store/revision:0021");
        assert!(serde_json::from_str::<Uid>(r#"""#).is_err());

        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["name"], "task-a");
        assert_eq!(json["resourceVersion"], "store/revision:0021");
        assert_eq!(json["generation"], 1);
        assert!(json["creationTimestamp"].is_string());
        assert!(json.get("uid").is_some());
    }

    #[test]
    fn creation_timestamp_uses_rfc3339_milliseconds_and_rejects_unix_numbers() {
        let mut meta = ObjectMeta::new(TaskId::new("task-a").unwrap()).unwrap();
        meta.set_resource_version("1").unwrap();
        let mut json = serde_json::to_value(meta).unwrap();
        json["creationTimestamp"] = serde_json::json!("2025-01-02T03:04:05.678Z");

        let back: ObjectMeta = serde_json::from_value(json).unwrap();
        let serialized = serde_json::to_value(back).unwrap();

        assert_eq!(serialized["creationTimestamp"], "2025-01-02T03:04:05.678Z");

        let meta = ObjectMeta::new(TaskId::new("task-a").unwrap()).unwrap();
        let mut json = serde_json::to_value(meta).unwrap();
        json["creationTimestamp"] = serde_json::json!(1_735_786_800_000_u64);

        assert!(serde_json::from_value::<ObjectMeta>(json).is_err());
    }
}
