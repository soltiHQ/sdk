use std::fmt;

use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

use crate::logger::object::timezone::get_or_detect_local_offset;

/// Dynamic RFC3339 timestamp formatter with local timezone support.
///
/// Reads the current local offset on every invocation, allowing timezone
/// changes to be reflected in logs without subscriber reinitialization.
///
/// Falls back to UTC if offset detection fails.
#[derive(Debug, Clone, Copy)]
pub struct LoggerRfc3339;

impl FormatTime for LoggerRfc3339 {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let local = OffsetDateTime::now_utc().to_offset(get_or_detect_local_offset());

        match local.format(&Rfc3339) {
            Ok(ts) => {
                write!(w, "{} ", ts)
            }
            Err(_) => {
                write!(w, "<invalid-time> ")
            }
        }
    }
}
