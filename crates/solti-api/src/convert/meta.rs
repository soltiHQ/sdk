//! # `ObjectMeta` domain → wire conversion.

use solti_model::ObjectMeta;

use super::time::system_time_to_ms;
use crate::proto_api;

impl From<&ObjectMeta> for proto_api::ObjectMeta {
    fn from(m: &ObjectMeta) -> Self {
        proto_api::ObjectMeta {
            id: m.id.to_string(),
            resource_version: m.resource_version,
            created_at: system_time_to_ms(m.created_at),
            updated_at: system_time_to_ms(m.updated_at),
        }
    }
}
