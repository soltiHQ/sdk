//! # Timestamp Conversion
//!
//! Protobuf v1 represents timestamps as Unix milliseconds.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::ApiError;

/// Converts a system timestamp into Unix milliseconds.
///
/// ## Errors
///
/// Returns [`ApiError::Internal`] when a response timestamp is before
/// the Unix epoch or outside the protobuf `int64` range.
pub(super) fn system_time_to_ms(t: SystemTime) -> Result<i64, ApiError> {
    let duration = t.duration_since(UNIX_EPOCH).map_err(|_| {
        ApiError::Internal("handler returned a timestamp before the Unix epoch".into())
    })?;
    i64::try_from(duration.as_millis()).map_err(|_| {
        ApiError::Internal("handler returned a timestamp outside the protobuf range".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn converts_supported_timestamp() {
        assert_eq!(
            system_time_to_ms(UNIX_EPOCH + Duration::from_millis(42)).unwrap(),
            42
        );
    }

    #[test]
    fn rejects_unsupported_timestamps() {
        for (timestamp, expected_message) in [
            (
                UNIX_EPOCH - Duration::from_millis(1),
                "before the Unix epoch",
            ),
            (
                UNIX_EPOCH + Duration::from_millis(i64::MAX as u64 + 1),
                "outside the protobuf range",
            ),
        ] {
            let error = system_time_to_ms(timestamp).unwrap_err();
            assert!(
                matches!(error, ApiError::Internal(message) if message.contains(expected_message))
            );
        }
    }
}
