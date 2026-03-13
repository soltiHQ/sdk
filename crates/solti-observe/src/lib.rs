//! Observability primitives for the Solti task execution system.
//!
//! ## Modules
//!
//! | Module                             | Feature         | Description                                              |
//! |------------------------------------|-----------------|----------------------------------------------------------|
//! | [`LoggerConfig`] / [`init_logger`] | -               | Configurable tracing subscriber (text / json / journald) |
//! | [`TracingEventSubscriber`]         | `subscriber`    | Logs taskvisor events via [`tracing`]                    |
//! | [`timezone_sync`]                  | `timezone-sync` | Periodic task that re-detects the local UTC offset       |
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use solti_observe::{LoggerConfig, LoggerLevel, init_local_offset, init_logger};
//!
//! // Must be called before spawning threads (tokio runtime).
//! init_local_offset();
//!
//! let cfg = LoggerConfig {
//!     level: LoggerLevel::new("info")?,
//!     ..Default::default()
//! };
//! init_logger(&cfg)?;
//! ```

mod logger;
pub use logger::*;

#[cfg(feature = "timezone-sync")]
pub use logger::timezone_sync;

mod subscriber;

#[cfg(feature = "subscriber")]
pub use subscriber::*;
