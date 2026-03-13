use std::fmt;

use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

use crate::logger::object::timezone::{LoggerTimeZone, get_or_detect_local_offset};

/// Dynamic RFC3339 timestamp formatter that respects [`LoggerTimeZone`].
///
/// - [`LoggerTimeZone::Utc`] — always formats as `…+00:00`
/// - [`LoggerTimeZone::Local`] — reads the cached local offset on every call,
///   so DST changes picked up by [`crate::timezone_sync`] are reflected
///   without subscriber reinitialization.
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
