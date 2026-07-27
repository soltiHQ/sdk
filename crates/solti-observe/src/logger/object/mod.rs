//! # Logger values
//!
//! These types validate serialized settings before [`init_logger`](crate::init_logger) runs.
//!
//! | Type               | Value                                  |
//! |--------------------|----------------------------------------|
//! | [`LoggerFormat`]   | Text, JSON, or optional journald       |
//! | [`LoggerLevel`]    | Validated `EnvFilter` expression       |
//! | [`LoggerTimeZone`] | UTC or cached local timestamp timezone |

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
