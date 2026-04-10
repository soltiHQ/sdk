//! # Logger value objects.
//!
//! Validated, serializable types used by [`LoggerConfig`](crate::LoggerConfig):
//!
//! - [`LoggerFormat`] output format (`Text`, `Json`, `Journald`).
//! - [`LoggerLevel`] validated `EnvFilter` expression wrapper.
//! - [`LoggerTimeZone`] timestamp timezone (`Utc`, `Local`).
//! - [`LoggerRfc3339`] RFC 3339 timestamp formatter for [`tracing_subscriber`].
//! - [`init_local_offset`] pre-runtime local UTC offset detection.

pub mod timezone;
pub use timezone::{LoggerTimeZone, init_local_offset};

pub mod rfc3339;
pub use rfc3339::LoggerRfc3339;

pub mod format;
pub use format::LoggerFormat;

pub mod level;
pub use level::LoggerLevel;
