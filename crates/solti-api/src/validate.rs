//! # Shared request validation.

use crate::error::ApiError;

/// Reject empty or whitespace-only string ids.
pub(crate) fn non_empty_id(field: &'static str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::InvalidRequest(format!("{field} cannot be empty")));
    }
    Ok(())
}

/// Reject an empty/whitespace slot string and hand the original back for use in builders.
///
/// Unlike [`non_empty_id`] _(which checks a borrowed reference)_, this takes ownership.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn validate_slot(slot: String) -> Result<String, ApiError> {
    non_empty_id("slot", &slot)?;
    Ok(slot)
}

/// Reject `timeout_ms == 0`.
#[cfg(any(feature = "grpc", feature = "http"))]
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
    fn non_empty_id_accepts_real_ids() {
        assert!(non_empty_id("task_id", "task-42").is_ok());
    }

    #[test]
    fn non_empty_id_rejects_empty() {
        let err = non_empty_id("task_id", "").unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(msg) if msg.contains("task_id")));
    }

    #[test]
    fn non_empty_id_rejects_whitespace() {
        assert!(non_empty_id("task_id", "   ").is_err());
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn validate_slot_accepts_real_slot() {
        assert_eq!(validate_slot("my-slot".into()).unwrap(), "my-slot");
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
        assert!(validate_slot("   ".into()).is_err());
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
    #[test]
    fn validate_timeout_accepts_positive() {
        assert_eq!(validate_timeout(5_000).unwrap(), 5_000);
    }

    #[cfg(any(feature = "grpc", feature = "http"))]
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
