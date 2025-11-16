use std::{fmt, str::FromStr, sync::{RwLock, OnceLock}};

use serde::{Deserialize, Serialize};
use time::UtcOffset;
use tracing::debug;

use crate::logger::error::LoggerError;

/// Global cache for the local UTC offset.
///
/// Updated by `init_local_offset()` on startup and `sync_local_offset()` periodically.
static LOCAL_OFFSET: RwLock<UtcOffset> = RwLock::new(UtcOffset::UTC);

/// Tracks whether local offset initialization has been attempted.
///
/// Set to `true` after first successful detection or explicit initialization.
static INIT_DONE: OnceLock<()> = OnceLock::new();

/// Timezone configuration for log timestamps.
///
/// - `Utc`: All timestamps in UTC (always works, default)
/// - `Local`: Uses system timezone
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum LoggerTimeZone {
    /// UTC timezone.
    Utc,
    /// Local system timezone.
    Local,
}

impl Default for LoggerTimeZone {
    fn default() -> Self {
        Self::Utc
    }
}

impl FromStr for LoggerTimeZone {
    type Err = LoggerError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalize = s.trim().to_ascii_lowercase();

        match normalize.as_str() {
            "utc" => Ok(Self::Utc),
            "local" => Ok(Self::Local),
            _ => Err(LoggerError::InvalidTimeZone(s.to_string())),
        }
    }
}

impl fmt::Display for LoggerTimeZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LoggerTimeZone::Utc => "utc",
            LoggerTimeZone::Local => "local",
        };
        f.write_str(s)
    }
}

/// Initializes local timezone offset early in the program.
///
/// **CRITICAL**: Call in `main()` **before spawning any threads** (before tokio runtime).
/// Timezone detection fails in multi-thread contexts on most Unix platforms.
///
/// Falls back to UTC silently if detection fails.
///
/// # Example
/// ```no_run
/// use tno_observe::init_local_offset;
/// use tokio;
///
/// fn main() {
///     init_local_offset();
///
///     tokio::runtime::Runtime::new()
///         .unwrap()
///         .block_on(async_main());
/// }
///
/// async fn async_main() {
///     // Async code
/// }
/// ```
pub fn init_local_offset() {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    *LOCAL_OFFSET.write().unwrap() = offset;
}

/// Synchronizes local offset.
pub(crate) fn sync_local_offset() -> Result<(), LoggerError> {
    let new_offset =
        UtcOffset::current_local_offset().map_err(|_| LoggerError::LocalTimezoneInitFailed)?;

    let mut guard = LOCAL_OFFSET.write().unwrap();
    let old_offset = *guard;

    if old_offset != new_offset {
        *guard = new_offset;
        debug!(
            "TZ offset updated: {} -> {}",
            format_offset(old_offset),
            format_offset(new_offset)
        );
    }
    Ok(())
}

/// Returns current local offset for timestamp formatting.
pub(crate) fn get_or_detect_local_offset() -> UtcOffset {
    INIT_DONE.get_or_init(|| {
        if let Ok(detected) = UtcOffset::current_local_offset() {
            *LOCAL_OFFSET.write().unwrap() = detected;
        }
    });
    *LOCAL_OFFSET.read().unwrap()
}

/// Formats offset as `UTC±HH` or `UTC±HH:MM`.
///
/// Examples: `"UTC+00"`, `"UTC+03:30"`, `"UTC-05"`
fn format_offset(offset: UtcOffset) -> String {
    let hours = offset.whole_hours();
    let minutes = offset.minutes_past_hour();
    if minutes == 0 {
        format!("UTC{:+03}", hours)
    } else {
        format!("UTC{:+03}:{:02}", hours, minutes.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_utc() {
        assert_eq!(LoggerTimeZone::default(), LoggerTimeZone::Utc);
    }

    #[test]
    fn parses_case_insensitive() {
        assert_eq!(
            LoggerTimeZone::from_str("utc").unwrap(),
            LoggerTimeZone::Utc
        );
        assert_eq!(
            LoggerTimeZone::from_str("UTC").unwrap(),
            LoggerTimeZone::Utc
        );
        assert_eq!(
            LoggerTimeZone::from_str("local").unwrap(),
            LoggerTimeZone::Local
        );
        assert_eq!(
            LoggerTimeZone::from_str("LOCAL").unwrap(),
            LoggerTimeZone::Local
        );
    }

    #[test]
    fn rejects_invalid_timezone() {
        assert!(LoggerTimeZone::from_str("").is_err());
        assert!(LoggerTimeZone::from_str("pst").is_err());
    }

    #[test]
    fn display_returns_canonical_names() {
        assert_eq!(LoggerTimeZone::Utc.to_string(), "utc");
        assert_eq!(LoggerTimeZone::Local.to_string(), "local");
    }

    #[test]
    fn format_offset_handles_utc() {
        assert_eq!(format_offset(UtcOffset::UTC), "UTC+00");
    }

    #[test]
    fn format_offset_handles_positive() {
        let offset = UtcOffset::from_hms(3, 30, 0).unwrap();
        assert_eq!(format_offset(offset), "UTC+03:30");
    }

    #[test]
    fn format_offset_handles_negative() {
        let offset = UtcOffset::from_hms(-5, 0, 0).unwrap();
        assert_eq!(format_offset(offset), "UTC-05");
    }

    #[test]
    fn get_after_init_returns_value() {
        init_local_offset();
        let offset = get_or_detect_local_offset();
        assert!(offset.whole_hours().abs() <= 14);
    }
}
