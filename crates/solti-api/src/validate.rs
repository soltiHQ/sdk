//! # Shared request validation.

use crate::error::ApiError;
use solti_model::{Slot, TaskId};

/// Parse one task resource name through the model-owned validation boundary.
pub(crate) fn parse_task_id(field: &'static str, value: String) -> Result<TaskId, ApiError> {
    let id = TaskId::from(value);
    id.validate_format()
        .map_err(|error| ApiError::InvalidRequest(format!("invalid {field}: {error}")))?;
    Ok(id)
}

/// Parse one slot through the model-owned validation boundary.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn validate_slot(slot: String) -> Result<Slot, ApiError> {
    let slot = Slot::from(slot);
    slot.validate_format()
        .map_err(|error| ApiError::InvalidRequest(format!("invalid slot: {error}")))?;
    Ok(slot)
}

/// Reject `timeout_ms == 0`.
#[cfg(feature = "grpc")]
pub(crate) fn validate_timeout(timeout_ms: u64) -> Result<u64, ApiError> {
    if timeout_ms == 0 {
        return Err(ApiError::InvalidRequest("timeout_ms cannot be zero".into()));
    }
    Ok(timeout_ms)
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn clamp_list_limit(raw: u32) -> usize {
    if raw == 0 {
        return solti_model::DEFAULT_LIMIT;
    }
    let bounded = usize::try_from(raw).unwrap_or(solti_model::MAX_LIMIT);
    bounded.min(solti_model::MAX_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_id_accepts_model_valid_names() {
        assert_eq!(
            parse_task_id("task name", "task-42".into())
                .unwrap()
                .as_str(),
            "task-42"
        );
    }

    #[test]
    fn parse_task_id_rejects_every_model_invalid_name() {
        for invalid in ["", "   ", "a/b", "a b", "."] {
            assert!(
                parse_task_id("task name", invalid.into()).is_err(),
                "must reject {invalid:?}"
            );
        }
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn validate_slot_accepts_real_slot() {
        assert_eq!(validate_slot("my-slot".into()).unwrap().as_str(), "my-slot");
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn validate_slot_rejects_empty() {
        let err = validate_slot(String::new()).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("slot")));
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn validate_slot_rejects_whitespace() {
        for invalid in ["   ", "a/b", "a b", "."] {
            assert!(validate_slot(invalid.into()).is_err());
        }
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn validate_timeout_accepts_positive() {
        assert_eq!(validate_timeout(5_000).unwrap(), 5_000);
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn validate_timeout_rejects_zero() {
        let err = validate_timeout(0).unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("timeout_ms")));
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn clamp_list_limit_zero_uses_default() {
        assert_eq!(clamp_list_limit(0), solti_model::DEFAULT_LIMIT);
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn clamp_list_limit_within_bounds_passes_through() {
        assert_eq!(clamp_list_limit(1), 1);
        assert_eq!(clamp_list_limit(50), 50);
        assert_eq!(
            clamp_list_limit(solti_model::MAX_LIMIT as u32),
            solti_model::MAX_LIMIT
        );
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn clamp_list_limit_above_cap_is_clamped() {
        assert_eq!(
            clamp_list_limit(solti_model::MAX_LIMIT as u32 + 1),
            solti_model::MAX_LIMIT
        );
        assert_eq!(clamp_list_limit(u32::MAX), solti_model::MAX_LIMIT);
    }
}
