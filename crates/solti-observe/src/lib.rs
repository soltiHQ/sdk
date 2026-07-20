//! Structured logging and timezone support for Solti agents.
//!
//! `solti-observe` is the logging crate for the Solti SDK.
//! It installs a [`tracing`] subscriber and keeps logger config in small value types.
//!
//! Use it when you build an agent binary and want one clear place for logs and local timestamps.
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
//!     tracing::info!("agent ready");
//!     Ok(())
//! }
//! ```
//!
//! [`init_logger`] installs the global tracing subscriber.
//! It can succeed only once in a process.
//!
//! ## Local Time
//!
//! UTC is the default and always works.
//! If you want local timestamps, call [`init_local_offset`] before Tokio starts worker threads:
//!
//! ```rust,no_run
//! use solti_observe::{
//!     LoggerConfig, LoggerTimeZone, init_local_offset, init_logger,
//! };
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     init_local_offset();
//!
//!     tokio::runtime::Runtime::new()?.block_on(async {
//!         let config = LoggerConfig {
//!             tz: LoggerTimeZone::Local,
//!             ..Default::default()
//!         };
//!         init_logger(&config)?;
//!         Ok(())
//!     })
//! }
//! ```
//!
//! ## What Ships
//!
//! | Item                  | Feature         | Use it for                                 |
//! |-----------------------|-----------------|--------------------------------------------|
//! | [`LoggerConfig`]      | always          | Logger settings with serde defaults        |
//! | [`LoggerFormat`]      | always          | `text`, `json`, or `journald`              |
//! | [`LoggerLevel`]       | always          | Validated `EnvFilter` strings              |
//! | [`LoggerTimeZone`]    | always          | `utc` or `local` timestamps                |
//! | [`init_logger`]       | always          | Install the global tracing subscriber      |
//! | [`init_local_offset`] | always          | Cache local UTC offset before Tokio starts |
//! | `timezone_sync`       | `timezone-sync` | Build a supervised offset refresh task     |
//!
//! ## Core Model
//!
//! ```text
//! LoggerConfig
//!   |
//!   v
//! init_logger()
//!   |
//!   |-- text logger
//!   |-- JSON logger
//!   |-- journald logger (Linux only)
//!   |
//!   v
//! tracing macros
//! ```
//!
//! The optional timezone task plugs into the same process:
//!
//! ```text
//! timezone_sync task -- refresh attempt --> local offset cache
//! ```
//!
//! ## Config
//!
//! [`LoggerConfig`] defaults to text logs, `info` level, UTC timestamps, targets enabled, and color enabled only when stdout is a terminal.
//!
//! ```rust
//! use solti_observe::{LoggerConfig, LoggerFormat};
//!
//! let config: LoggerConfig = serde_json::from_str(r#"{
//!     "format": "json",
//!     "level": "solti_core=debug,info"
//! }"#).unwrap();
//!
//! assert_eq!(config.format, LoggerFormat::Json);
//! assert_eq!(config.level.as_str(), "solti_core=debug,info");
//! ```
//!
//! ## Timezone Sync
//!
//! Enable the `timezone-sync` feature to use `timezone_sync()`.
//! It builds a periodic supervised task that tries to refresh the local UTC offset.
//! The startup call to [`init_local_offset`] is still the important part on most platforms.
//!
//! ## Also
//!
//! - [`tracing`] is the structured logging framework used underneath.
//! - `solti-prometheus` is the metrics crate for the same event stream.
//! - See `examples/agentd-http` and `examples/agentd-grpc` for full agent wiring.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod logger;
pub use logger::{
    LoggerConfig, LoggerError, LoggerFormat, LoggerLevel, LoggerTimeZone, init_local_offset,
    init_logger,
};

/// Build the periodic timezone refresh task.
#[cfg(feature = "timezone-sync")]
pub use logger::timezone_sync;
