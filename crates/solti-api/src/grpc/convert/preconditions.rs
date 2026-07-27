//! Write precondition protobuf conversion.

use solti_model::{Uid, WritePreconditions};

use crate::{ApiError, proto_api};

pub(crate) fn write_preconditions_from_proto(
    value: Option<proto_api::WritePreconditions>,
) -> Result<WritePreconditions, ApiError> {
    let Some(value) = value else {
        return Ok(WritePreconditions::new());
    };

    let mut preconditions = WritePreconditions::new();
    if let Some(uid) = value.uid {
        preconditions = preconditions.with_uid(Uid::new(uid).map_err(|error| {
            ApiError::InvalidRequest(format!("invalid preconditions.uid: {error}"))
        })?);
    }
    if let Some(resource_version) = value.resource_version {
        preconditions = preconditions
            .with_resource_version(resource_version)
            .map_err(|error| {
                ApiError::InvalidRequest(format!("invalid preconditions.resourceVersion: {error}"))
            })?;
    }
    Ok(preconditions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_is_unconditional() {
        assert!(write_preconditions_from_proto(None).unwrap().is_empty());
    }

    #[test]
    fn converts_both_fields() {
        let preconditions = write_preconditions_from_proto(Some(proto_api::WritePreconditions {
            uid: Some("uid-1".into()),
            resource_version: Some("12".into()),
        }))
        .unwrap();

        assert_eq!(preconditions.uid().unwrap().as_str(), "uid-1");
        assert_eq!(preconditions.resource_version(), Some("12"));
    }

    #[test]
    fn rejects_present_empty_fields() {
        let uid = write_preconditions_from_proto(Some(proto_api::WritePreconditions {
            uid: Some(String::new()),
            resource_version: None,
        }))
        .unwrap_err();
        assert!(matches!(uid, ApiError::InvalidRequest(message) if message.contains("uid")));

        let version = write_preconditions_from_proto(Some(proto_api::WritePreconditions {
            uid: None,
            resource_version: Some(String::new()),
        }))
        .unwrap_err();
        assert!(
            matches!(version, ApiError::InvalidRequest(message) if message.contains("resourceVersion"))
        );
    }
}
