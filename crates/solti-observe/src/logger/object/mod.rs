//! Logger value objects.
//!
//! These small types keep logger config valid before [`init_logger`](crate::init_logger) installs the subscriber.
//!
//! - [`LoggerRfc3339`] RFC 3339 timestamp formatter for [`tracing_subscriber`].
//! - [`init_local_offset`] pre-runtime local UTC offset detection.
//! - [`LoggerFormat`] output format (`Text`, `Json`, `Journald`).
//! - [`LoggerLevel`] validated `EnvFilter` expression wrapper.
//! - [`LoggerTimeZone`] timestamp timezone (`Utc`, `Local`).

pub mod timezone;
pub use timezone::{LoggerTimeZone, init_local_offset};

pub mod rfc3339;
pub use rfc3339::LoggerRfc3339;

pub mod format;
pub use format::LoggerFormat;

pub mod level;
pub use level::LoggerLevel;
