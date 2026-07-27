//! # solti-observe
//!
//! Structured logging for Solti binaries.
//! The crate installs one global [`tracing`] subscriber from [`LoggerConfig`].
//!
//! ## Start Here
//!
//! ```rust,no_run
//! use solti_observe::{LoggerConfig, LoggerLevel, init_logger};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = LoggerConfig {
//!         level: LoggerLevel::new("taskvisor=debug,info")?,
//!         ..Default::default()
//!     };
//!
//!     init_logger(&config)?;
//!     tracing::info!("agent ready");
//!     Ok(())
//! }
//! ```
//!
//! [`init_logger`] can succeed only once in a process.
//!
//! ## Formats
//!
//! | Format     | Feature    | Output                         |
//! |------------|------------|--------------------------------|
//! | `text`     | always     | Human-readable lines           |
//! | `json`     | always     | Structured JSON                |
//! | `journald` | `journald` | Native systemd journal records |
//!
//! Journald initialization is supported on Linux.
//!
//! ## Local Time
//!
//! UTC is the default. Local time is explicit:
//!
//! ```rust,no_run
//! use solti_observe::{LoggerConfig, LoggerError, LoggerTimeZone, init_logger};
//!
//! # fn main() -> Result<(), LoggerError> {
//! init_logger(&LoggerConfig {
//!     timezone: LoggerTimeZone::Local,
//!     ..Default::default()
//! })?;
//! # Ok(()) }
//! ```
//!
//! Logger initialization returns [`LoggerError::LocalOffsetUnavailable`] when
//! the system offset cannot be determined. It never silently changes `local`
//! to UTC.
//!
//! Feature `timezone-sync` adds a periodic supervised task that refreshes the
//! cached offset. A failed refresh follows its Taskvisor retry policy.
//!
//! ## Feature Flags
//!
//! - `journald`: native journald output.
//! - `log-compat`: forwards records from the `log` facade into `tracing`.
//! - `timezone-sync`: supervised local-offset refresh task.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod logger;
pub use logger::{
    LoggerConfig, LoggerError, LoggerFormat, LoggerLevel, LoggerTimeZone, init_logger,
};

/// Build the periodic timezone refresh task.
#[cfg(feature = "timezone-sync")]
pub use logger::timezone_sync;
