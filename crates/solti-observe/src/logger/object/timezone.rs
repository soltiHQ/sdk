//! # Local-timezone offset cache ([`LoggerTimeZone`]).

use std::{
    fmt,
    str::FromStr,
    sync::{
        OnceLock,
        atomic::{AtomicI32, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use time::UtcOffset;
#[cfg(feature = "timezone-sync")]
use tracing::debug;

use crate::logger::error::LoggerError;

/// Global cache for the local UTC offset, stored as whole seconds (0 = UTC).
///
/// Updated by `init_local_offset()` on startup and `sync_local_offset()` periodically.
///
/// An atomic (instead of a lock) keeps the per-log-line read wait-free and
/// re-entrancy safe: `sync_local_offset()` emits a `debug!` that the fmt layer
/// formats synchronously on the same thread, reading this cache. With a lock
/// held across that log call the read would self-deadlock.
static LOCAL_OFFSET_SECONDS: AtomicI32 = AtomicI32::new(0);

/// Tracks whether local offset initialization has been attempted.
///
/// Set to `true` after first successful detection or explicit initialization.
static INIT_DONE: OnceLock<()> = OnceLock::new();

/// Timezone configuration for log timestamps.
///
/// Controls which UTC offset is applied to RFC 3339 timestamps in log output.
///
/// ## Variants
///
/// | Variant | Timestamps look like         | Requirement                        |
/// |---------|------------------------------|------------------------------------|
/// | `Utc`   | `2025-01-15T10:30:00+00:00`  | None (always works)                |
/// | `Local` | `2025-01-15T13:30:00+03:00`  | [`init_local_offset`] before tokio |
///
/// ## Default
///
/// Defaults to `Utc`.
///
/// ## Parsing
///
/// Case-insensitive [`FromStr`]: `"utc"`, `"UTC"`, `"local"`, `"LOCAL"`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub enum LoggerTimeZone {
    /// UTC timezone.
    Utc,
    /// Local system timezone.
    ///
    /// Requires [`init_local_offset`] to be called in `main()` before
    /// spawning the tokio runtime. Without it, falls back to UTC with
    /// a warning on stderr.
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

/// Detects the local UTC offset and caches it for timestamp formatting.
///
/// **Must be called in `main()` before spawning the tokio runtime.**
///
/// ## Why
///
/// On most Unix platforms, reading `/etc/localtime` is only safe in a single-threaded process.
/// Once tokio spawns its worker threads, the `time` crate's `UtcOffset::current_local_offset()`
/// returns `Err`. Timestamps then silently fall back to UTC.
///
/// ## Behaviour
///
/// - Caches the detected offset in a global atomic (whole seconds).
/// - Falls back to UTC silently if detection fails.
/// - Idempotent - safe to call multiple times; only the first call
///   triggers detection.
///
/// ## Example
///
/// ```no_run
/// use solti_observe::init_local_offset;
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
///     // timestamps will use local timezone
/// }
/// ```
pub fn init_local_offset() {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    LOCAL_OFFSET_SECONDS.store(offset.whole_seconds(), Ordering::Relaxed);
    let _ = INIT_DONE.set(());
}

/// Re-detects the system UTC offset and updates the global cache.
///
/// Called periodically by the [`crate::timezone_sync`] task. The cache is
/// updated with a single atomic store **before** the `debug!` below, so the
/// fmt layer can read the fresh value while formatting that very log line.
///
/// ## Errors
///
/// Currently always returns `Ok`. Detection fails whenever the process is
/// multi-threaded (on most Unix platforms), which is the normal state under
/// tokio - the sync is then skipped with a debug log and the cache keeps the
/// offset detected by [`init_local_offset`].
#[cfg(feature = "timezone-sync")]
pub(crate) fn sync_local_offset() -> Result<(), LoggerError> {
    match UtcOffset::current_local_offset() {
        Ok(new_offset) => {
            let new_seconds = new_offset.whole_seconds();
            let old_seconds = LOCAL_OFFSET_SECONDS.swap(new_seconds, Ordering::Relaxed);
            if old_seconds != new_seconds {
                debug!(
                    "TZ offset updated: {} -> {}",
                    format_offset(offset_from_seconds(old_seconds)),
                    format_offset(new_offset)
                );
            }
            Ok(())
        }
        Err(_) => {
            debug!("Timezone sync skipped (multi-thread context)");
            Ok(())
        }
    }
}

/// Returns the cached local offset for timestamp formatting.
///
/// On first call (if [`init_local_offset`] was never called) attempts a one-shot detection.
/// On failure prints a warning to stderr and falls back to UTC.
///
/// Subsequent calls are a single atomic load - safe on the logging hot path.
pub(crate) fn get_or_detect_local_offset() -> UtcOffset {
    INIT_DONE.get_or_init(|| match UtcOffset::current_local_offset() {
        Ok(detected) => {
            LOCAL_OFFSET_SECONDS.store(detected.whole_seconds(), Ordering::Relaxed);
        }
        Err(_) => {
            eprintln!(
                "WARNING: solti-observe local timezone detection failed. \
                          Call init_local_offset() in main() before tokio runtime. \
                          Falling back to UTC."
            );
        }
    });

    offset_from_seconds(LOCAL_OFFSET_SECONDS.load(Ordering::Relaxed))
}

/// Converts cached whole seconds back into a [`UtcOffset`].
///
/// The cache only ever holds values produced by `UtcOffset::whole_seconds()`,
/// so the conversion cannot fail; UTC is a defensive fallback rather than a
/// reachable branch.
fn offset_from_seconds(seconds: i32) -> UtcOffset {
    UtcOffset::from_whole_seconds(seconds).unwrap_or(UtcOffset::UTC)
}

/// Formats offset as `UTC±HH` or `UTC±HH:MM`.
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

    // Single test for everything touching the global offset cache: separate
    // #[test] functions run on parallel threads and would race on the statics.
    #[test]
    fn offset_cache_updates_are_visible() {
        init_local_offset();
        let offset = get_or_detect_local_offset();
        assert!(offset.whole_hours().abs() <= 14);

        // An atomic store (as done by sync_local_offset) is immediately
        // visible to readers; INIT_DONE is already set, so no re-detection
        // overwrites it.
        let positive = UtcOffset::from_hms(3, 30, 0).unwrap();
        LOCAL_OFFSET_SECONDS.store(positive.whole_seconds(), Ordering::Relaxed);
        assert_eq!(get_or_detect_local_offset(), positive);

        let negative = UtcOffset::from_hms(-5, -45, 0).unwrap();
        LOCAL_OFFSET_SECONDS.store(negative.whole_seconds(), Ordering::Relaxed);
        assert_eq!(get_or_detect_local_offset(), negative);

        // Restore the real local offset for any later reader.
        init_local_offset();
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
