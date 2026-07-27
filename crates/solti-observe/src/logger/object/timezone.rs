//! Local timezone offset cache.

use std::{
    fmt,
    str::FromStr,
    sync::atomic::{AtomicI32, Ordering},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::{UtcOffset, error::IndeterminateOffset};
#[cfg(feature = "timezone-sync")]
use tracing::debug;

use crate::logger::error::LoggerError;

/// Global cache for the local UTC offset, stored as whole seconds (0 = UTC).
///
/// Updated by [`initialize_local_offset`] and [`sync_local_offset`].
static LOCAL_OFFSET_SECONDS: AtomicI32 = AtomicI32::new(0);

/// Timezone setting for log timestamps.
///
/// UTC is the default. [`crate::init_logger`] initializes the local offset
/// before installing a logger configured with [`Local`](Self::Local).
///
/// # Example
///
/// ```
/// use solti_observe::LoggerTimeZone;
///
/// let tz: LoggerTimeZone = "local".parse().unwrap();
/// assert_eq!(tz, LoggerTimeZone::Local);
/// assert_eq!(LoggerTimeZone::Utc.to_string(), "utc");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggerTimeZone {
    /// UTC timestamps.
    Utc,
    /// Local system timezone.
    ///
    /// Logger initialization fails when the system offset cannot be determined.
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

impl Serialize for LoggerTimeZone {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LoggerTimeZone {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_str(&value).map_err(serde::de::Error::custom)
    }
}

/// Detect and cache the current local UTC offset.
pub(crate) fn initialize_local_offset() -> Result<(), LoggerError> {
    refresh_local_offset_with(UtcOffset::current_local_offset).map(|_| ())
}

/// Re-detect the system UTC offset and update the global cache.
///
/// Called periodically by the [`crate::timezone_sync`] task.
/// The cache is updated with a single atomic store **before** the `debug!` below;
/// the fmt layer can read the fresh value while formatting that very log line.
///
#[cfg(feature = "timezone-sync")]
pub(crate) fn sync_local_offset() -> Result<(), LoggerError> {
    let (old_offset, new_offset) = refresh_local_offset_with(UtcOffset::current_local_offset)?;
    if old_offset != new_offset {
        debug!(
            old_offset = %format_offset(old_offset),
            new_offset = %format_offset(new_offset),
            "timezone offset updated"
        );
    }
    Ok(())
}

/// Return the cached local offset.
pub(crate) fn local_offset() -> UtcOffset {
    offset_from_seconds(LOCAL_OFFSET_SECONDS.load(Ordering::Relaxed))
}

fn refresh_local_offset_with(
    detect: impl FnOnce() -> Result<UtcOffset, IndeterminateOffset>,
) -> Result<(UtcOffset, UtcOffset), LoggerError> {
    let new_offset = detect().map_err(LoggerError::LocalOffsetUnavailable)?;
    let old_seconds = LOCAL_OFFSET_SECONDS.swap(new_offset.whole_seconds(), Ordering::Relaxed);
    Ok((offset_from_seconds(old_seconds), new_offset))
}

/// Convert cached whole seconds back into a [`UtcOffset`].
///
/// The cache only ever holds values produced by `UtcOffset::whole_seconds()`;
/// the conversion cannot fail;
/// UTC is a defensive fallback rather than a reachable branch.
fn offset_from_seconds(seconds: i32) -> UtcOffset {
    UtcOffset::from_whole_seconds(seconds).unwrap_or(UtcOffset::UTC)
}

/// Format offset as `UTC+HH` or `UTC+HH:MM`.
///
/// Examples: `"UTC+00"`, `"UTC+03:30"`, `"UTC-05"`
#[cfg(feature = "timezone-sync")]
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
    fn serde_uses_canonical_lowercase_names() {
        assert_eq!(
            serde_json::to_string(&LoggerTimeZone::Utc).unwrap(),
            r#""utc""#
        );
        assert_eq!(
            serde_json::from_str::<LoggerTimeZone>(r#""LOCAL""#).unwrap(),
            LoggerTimeZone::Local
        );
    }

    #[test]
    fn detection_failure_is_not_hidden_as_utc() {
        let error = refresh_local_offset_with(|| Err(IndeterminateOffset)).unwrap_err();
        assert!(matches!(error, LoggerError::LocalOffsetUnavailable(_)));
    }

    #[cfg(feature = "timezone-sync")]
    #[test]
    fn format_offset_handles_utc() {
        assert_eq!(format_offset(UtcOffset::UTC), "UTC+00");
    }

    #[cfg(feature = "timezone-sync")]
    #[test]
    fn format_offset_handles_positive() {
        let offset = UtcOffset::from_hms(3, 30, 0).unwrap();
        assert_eq!(format_offset(offset), "UTC+03:30");
    }

    #[cfg(feature = "timezone-sync")]
    #[test]
    fn format_offset_handles_negative() {
        let offset = UtcOffset::from_hms(-5, 0, 0).unwrap();
        assert_eq!(format_offset(offset), "UTC-05");
    }

    #[test]
    fn offset_cache_updates_are_visible() {
        let positive = UtcOffset::from_hms(3, 30, 0).unwrap();
        LOCAL_OFFSET_SECONDS.store(positive.whole_seconds(), Ordering::Relaxed);
        assert_eq!(local_offset(), positive);

        let negative = UtcOffset::from_hms(-5, -45, 0).unwrap();
        LOCAL_OFFSET_SECONDS.store(negative.whole_seconds(), Ordering::Relaxed);
        assert_eq!(local_offset(), negative);
    }

    #[test]
    fn offset_seconds_roundtrip() {
        for offset in [
            UtcOffset::UTC,
            UtcOffset::from_hms(14, 0, 0).unwrap(),
            UtcOffset::from_hms(-12, 0, 0).unwrap(),
            UtcOffset::from_hms(5, 45, 0).unwrap(),
        ] {
            assert_eq!(offset_from_seconds(offset.whole_seconds()), offset);
        }
    }
}
