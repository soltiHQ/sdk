//! # RFC 3339 timestamps
//!
//! The logger injects [`Rfc3339Timer`] into `tracing-subscriber`.
//! It formats timestamps in UTC or with the cached local offset.
//!
//! ```text
//! current UTC time + selected offset ──► RFC 3339 timestamp
//! ```
//!
//! A formatting failure writes `<invalid-time>`.

use std::fmt;

use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

use crate::logger::object::timezone::{LoggerTimeZone, local_offset};

/// RFC 3339 timestamp formatter used by the text and JSON loggers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rfc3339Timer {
    tz: LoggerTimeZone,
}

impl Rfc3339Timer {
    /// Creates a formatter for one timezone setting.
    pub(crate) fn new(tz: LoggerTimeZone) -> Self {
        Self { tz }
    }
}

impl FormatTime for Rfc3339Timer {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let offset = match self.tz {
            LoggerTimeZone::Utc => UtcOffset::UTC,
            LoggerTimeZone::Local => local_offset(),
        };

        let ts = OffsetDateTime::now_utc().to_offset(offset);

        match ts.format(&Rfc3339) {
            Ok(s) => w.write_str(&s),
            Err(_) => w.write_str("<invalid-time>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_timestamp_has_no_embedded_separator() {
        let timer = Rfc3339Timer::new(LoggerTimeZone::Utc);
        let mut rendered = String::new();

        timer.format_time(&mut Writer::new(&mut rendered)).unwrap();

        assert_eq!(rendered.trim(), rendered);
        assert!(rendered.ends_with('Z') || rendered.ends_with("+00:00"));
    }
}
