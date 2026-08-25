//! # solti-observe
//!
//! Shared logging configuration for Solti binaries.
//! It validates settings and installs one global [`tracing`] subscriber.
//!
//! ## Start Here
//!
//! 1. Create a [`LoggerConfig`].
//! 2. Choose a [`LoggerFormat`], [`LoggerLevel`], and [`LoggerTimeZone`].
//! 3. Call [`init_logger`] once near process start.
//! 4. Emit records through `tracing`.
//!
//! ## Flow
//!
//! ```text
//! config file ──► LoggerConfig ──► init_logger()
//!                                      │
//!                    ┌─────────────────┼──────────────────┐
//!                    ▼                 ▼                  ▼
//!                  text              JSON             journald
//!                    │                 │                  │
//!                    └─────────────────┴──────────────────┘
//!                                      │
//!                                      ▼
//!                              global subscriber
//! ```
//!
//! Journald uses its native record fields and the configured level filter.
//! Text and JSON use the configured timestamp and target settings.
//! Text can also use ANSI colors.
//!
//! ## Quick Start
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
//!     tracing::info!(event = "service.ready", "agent ready");
//!     Ok(())
//! }
//! ```
//!
//! Only one global installation can succeed.
//!
//! ## Formats
//!
//! | Format                       | Feature    | Output                         |
//! |------------------------------|------------|--------------------------------|
//! | [`Text`](LoggerFormat::Text) | Always     | Human-readable lines           |
//! | [`Json`](LoggerFormat::Json) | Always     | Structured JSON                |
//! | `Journald`                   | `journald` | Native systemd journal records |
//!
//! Text output uses ANSI colors only when requested and stdout is a terminal.
//! Journald initialization is available only on Linux.
//! JSON output never uses ANSI colors.
//!
//! ## Filtering
//!
//! [`LoggerLevel`] validates a `tracing_subscriber::EnvFilter` expression.
//! It preserves the original string for configuration round trips.
//!
//! ## Local Time
//!
//! UTC is the default.
//! Text and JSON output can use the current local UTC offset:
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
//! [`init_logger`] detects the local offset first.
//! It then installs the text or JSON subscriber.
//! It returns [`LoggerError::LocalOffsetUnavailable`] when detection fails.
//! It does not replace a requested local timezone with UTC.
//!
//! The offset stays cached after initialization.
//! Feature `timezone-sync` adds a supervised task that refreshes it.
//!
//! ## Timezone Refresh
//!
//! ```text
//! current system offset ──► timezone_sync task ──► atomic offset cache
//!                                                       ▼
//!                                                  text / JSON timer
//! ```
//!
//! The task runs hourly after success.
//! Failed detection uses exponential retry backoff.
//!
//! ## Feature Flags
//!
//! - `journald`: native journald output.
//! - `log-compat`: forwards records from the `log` facade into `tracing`.
//! - `timezone-sync`: supervised local-offset refresh task.
//! - `full`: enables every optional integration.
//!
//! ## Main Types
//!
//! | Type               | Purpose                               |
//! |--------------------|---------------------------------------|
//! | [`LoggerConfig`]   | Complete serializable logger settings |
//! | [`LoggerFormat`]   | Output backend selection              |
//! | [`LoggerLevel`]    | Validated event filter                |
//! | [`LoggerTimeZone`] | UTC or cached local timestamps        |
//! | [`LoggerError`]    | Validation and initialization errors  |

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod logger;
pub use logger::{
    LoggerConfig, LoggerError, LoggerFormat, LoggerLevel, LoggerTimeZone, init_logger,
};

/// Builds the periodic timezone refresh task.
#[cfg(feature = "timezone-sync")]
#[cfg_attr(docsrs, doc(cfg(feature = "timezone-sync")))]
pub use logger::timezone_sync;
