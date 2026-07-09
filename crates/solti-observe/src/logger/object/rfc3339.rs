//! RFC 3339 timestamp formatter.
//!
//! The logger injects [`LoggerRfc3339`] into `tracing-subscriber`.
//! It formats timestamps in UTC or with the cached local offset.

use std::fmt;

use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

use crate::logger::object::timezone::{LoggerTimeZone, get_or_detect_local_offset};

/// RFC 3339 timestamp formatter used by the text and JSON loggers.
///
/// It implements [`FormatTime`](tracing_subscriber::fmt::time::FormatTime) and is injected into the `fmt::Layer` by [`crate::init_logger`].
///
/// ## Behaviour
///
/// | Timezone                  | Offset source                 | Example output                 |
/// |---------------------------|-------------------------------|--------------------------------|
/// | [`LoggerTimeZone::Utc`]   | Always `+00:00`               | `2025-01-15T10:30:00+00:00`    |
/// | [`LoggerTimeZone::Local`] | Cached local offset (per-call)| `2025-01-15T13:30:00+03:00`    |
///
/// In `Local` mode the cached offset is read on every call with one atomic load and no lock.
/// If the cache changes, new log lines use the new offset.
///
/// On most Unix platforms, later re-detection fails after the process becomes multi-threaded, so the startup value from
/// [`init_local_offset`](crate::init_local_offset) is the important one.
///
/// ## Also
///
/// - [`init_local_offset`](crate::init_local_offset) - must be called before tokio for `Local` mode.
/// - [`LoggerTimeZone`] - timezone configuration.
#[derive(Debug, Clone, Copy)]
pub struct LoggerRfc3339 {
    tz: LoggerTimeZone,
}

impl LoggerRfc3339 {
    /// Create a formatter for the given timezone setting.
    pub fn new(tz: LoggerTimeZone) -> Self {
        Self { tz }
    }
}

impl FormatTime for LoggerRfc3339 {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let offset = match self.tz {
            LoggerTimeZone::Utc => UtcOffset::UTC,
            LoggerTimeZone::Local => get_or_detect_local_offset(),
        };

        let ts = OffsetDateTime::now_utc().to_offset(offset);

        match ts.format(&Rfc3339) {
            Ok(s) => write!(w, "{s} "),
            Err(_) => write!(w, "<invalid-time> "),
        }
    }
}
