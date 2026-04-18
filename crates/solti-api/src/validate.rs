//! # Shared request validation.
//!
//! Transport-agnostic checks reused by both gRPC and HTTP handlers so
//! invariants not expressible in proto (trimmed, non-empty strings) are
//! enforced in one place.

use crate::error::ApiError;

/// Reject empty or whitespace-only string ids.
pub(crate) fn non_empty_id(field: &'static str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() {
        return Err(ApiError::InvalidRequest(format!("{field} cannot be empty")));
    }
    Ok(())
}
