//! # Request Validation
//!
//! Shared validation used by HTTP and gRPC.
//! Model-owned values are constructed before handler calls.

use crate::error::ApiError;
use solti_model::{Slot, TaskId};

/// Parses a task name through [`TaskId`].
pub(crate) fn parse_task_id(field: &'static str, value: String) -> Result<TaskId, ApiError> {
    TaskId::new(value)
        .map_err(|error| ApiError::InvalidRequest(format!("invalid {field}: {error}")))
}

/// Parses a slot through [`Slot`].
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn validate_slot(slot: String) -> Result<Slot, ApiError> {
    Slot::new(slot).map_err(|error| ApiError::InvalidRequest(format!("invalid slot: {error}")))
}

/// Accepts a positive protobuf timeout in milliseconds.
#[cfg(feature = "grpc")]
pub(crate) fn validate_timeout(timeout_ms: u64) -> Result<u64, ApiError> {
    if timeout_ms == 0 {
        return Err(ApiError::InvalidRequest("timeout_ms cannot be zero".into()));
    }
    Ok(timeout_ms)
}

#[cfg(any(feature = "grpc", feature = "http"))]
/// Applies the public default and maximum page size.
pub(crate) fn parse_list_limit(raw: u32) -> Result<usize, ApiError> {
    if raw == 0 {
        return Ok(solti_model::DEFAULT_LIMIT);
    }
    let limit = usize::try_from(raw)
        .map_err(|_| ApiError::InvalidRequest(format!("limit `{raw}` is out of range")))?;
    if limit > solti_model::MAX_LIMIT {
        return Err(ApiError::InvalidRequest(format!(
            "limit cannot exceed {}",
            solti_model::MAX_LIMIT
        )));
    }
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_id_uses_model_validation() {
        assert_eq!(
            parse_task_id("task name", "task-42".into())
                .unwrap()
                .as_str(),
            "task-42"
        );

        for invalid in ["", "   ", "a/b", "a b", "."] {
            assert!(
                parse_task_id("task name", invalid.into()).is_err(),
                "must reject {invalid:?}"
            );
        }
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn validate_slot_uses_model_validation() {
        assert_eq!(validate_slot("my-slot".into()).unwrap().as_str(), "my-slot");

        let err = validate_slot(String::new()).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("slot")));

        for invalid in ["   ", "a/b", "a b", "."] {
            assert!(validate_slot(invalid.into()).is_err());
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn validate_timeout_requires_a_positive_value() {
        assert_eq!(validate_timeout(5_000).unwrap(), 5_000);

        let err = validate_timeout(0).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("timeout_ms")));
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn parse_list_limit_applies_defaults_and_bounds() {
        assert_eq!(parse_list_limit(0).unwrap(), solti_model::DEFAULT_LIMIT);

        assert_eq!(parse_list_limit(1).unwrap(), 1);
        assert_eq!(parse_list_limit(50).unwrap(), 50);
        assert_eq!(
            parse_list_limit(solti_model::MAX_LIMIT as u32).unwrap(),
            solti_model::MAX_LIMIT
        );

        assert!(parse_list_limit(solti_model::MAX_LIMIT as u32 + 1).is_err());
        assert!(parse_list_limit(u32::MAX).is_err());
    }
}
