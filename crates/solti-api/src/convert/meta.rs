//! # `ObjectMeta` domain to wire conversion.

use solti_model::ObjectMeta;

use super::time::system_time_to_ms;
use crate::proto_api;

impl From<&ObjectMeta> for proto_api::ObjectMeta {
    fn from(m: &ObjectMeta) -> Self {
        proto_api::ObjectMeta {
            name: m.name().to_string(),
            uid: m.uid().to_string(),
            resource_version: m.resource_version().to_owned(),
            generation: m.generation(),
            creation_timestamp: system_time_to_ms(m.creation_timestamp()),
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
        }
    }
}
