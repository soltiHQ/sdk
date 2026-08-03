//! # Object Metadata Conversion
//!
//! Converts domain object metadata into protobuf response values.

use solti_model::ObjectMeta;

use super::time::system_time_to_ms;
use crate::error::ApiError;
use crate::proto_api;

impl TryFrom<&ObjectMeta> for proto_api::ObjectMeta {
    type Error = ApiError;

    fn try_from(m: &ObjectMeta) -> Result<Self, Self::Error> {
        Ok(proto_api::ObjectMeta {
            name: m.name().to_string(),
            uid: m.uid().to_string(),
            resource_version: m.resource_version().to_owned(),
            generation: m.generation(),
            creation_timestamp: system_time_to_ms(m.creation_timestamp())?,
            labels: m
                .labels()
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            annotations: m
                .annotations()
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
        })
    }
}
