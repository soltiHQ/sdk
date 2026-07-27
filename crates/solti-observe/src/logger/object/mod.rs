//! Logger value objects.
//!
//! These small types keep logger config valid before [`init_logger`](crate::init_logger) installs the subscriber.
//!
//! - [`LoggerFormat`] output format (`Text`, `Json`, and optional `Journald`).
//! - [`LoggerLevel`] validated `EnvFilter` expression wrapper.
//! - [`LoggerTimeZone`] timestamp timezone (`Utc`, `Local`).

mod timezone;
pub use timezone::LoggerTimeZone;
pub(crate) use timezone::initialize_local_offset;
#[cfg(feature = "timezone-sync")]
pub(crate) use timezone::sync_local_offset;

mod rfc3339;
pub(crate) use rfc3339::Rfc3339Timer;

mod format;
pub use format::LoggerFormat;

mod level;
pub use level::LoggerLevel;
