//! # solti-observe
//!
//! Observability primitives for the solti task execution system.
//!
//! This crate wires [`tracing`] into solti, covering three concerns:
//!
//! 1. **Logger initialization** - [`init_logger`] installs a global tracing
//!    subscriber configured via [`LoggerConfig`] (format, level filter,
//!    timezone, color).
//! 2. **Event logging** - `TracingBridge` (feature `subscriber`, re-exported
//!    from `taskvisor`) forwards every supervision event to tracing with
//!    structured fields and stable `event` labels.
//! 3. **Timezone sync** - `timezone_sync` (feature `timezone-sync`) is a
//!    periodic task that re-detects the local UTC offset. This keeps log
//!    timestamps correct across DST transitions.
//!
//! ## Architecture
//!
//! ```text
//!  main()
//!  ├─ init_local_offset()              // before tokio runtime
//!  └─ tokio::Runtime::new()
//!      └─ async_main()
//!          ├─ init_logger(&cfg)        // installs global tracing subscriber
//!          │   ├─ Text  → fmt::Layer (colored, RFC 3339 timestamps)
//!          │   ├─ Json  → fmt::Layer::json()
//!          │   └─ Journald → tracing_journald::layer() (Linux only)
//!          │
//!          ├─ TracingBridge            // feature: subscriber
//!          │   └─ on_event() → tracing events, target "taskvisor"
//!          │
//!          └─ timezone_sync()          // feature: timezone-sync
//!              └─ periodic re-detection of local UTC offset
//!  ```
//!
//! ## Public API
//!
//! | Item                                     | Feature         | Description                                              |
//! |------------------------------------------|-----------------|----------------------------------------------------------|
//! | [`LoggerConfig`]                         | -               | Logger configuration (format, level, timezone, color)    |
//! | [`init_logger`]                          | -               | Install global tracing subscriber                        |
//! | [`init_local_offset`]                    | -               | Detect local UTC offset (call before tokio runtime)      |
//! | [`LoggerFormat`]                         | -               | Output format: `Text` / `Json` / `Journald`              |
//! | [`LoggerLevel`]                          | -               | Validated `EnvFilter` expression wrapper                 |
//! | [`LoggerTimeZone`]                       | -               | Timestamp timezone: `Utc` / `Local`                      |
//! | [`LoggerError`]                          | -               | Error type for logger initialization                     |
//! | `TracingBridge`                        | `subscriber`    | Logs `taskvisor` events via tracing                    |
//! | `timezone_sync`                        | `timezone-sync` | Periodic task that re-detects the local UTC offset       |
//!
//! ## Feature flags
//!
//! | Flag            | Default | Dependencies                        | Effect                                        |
//! |-----------------|---------|-------------------------------------|-----------------------------------------------|
//! | `subscriber`    | off     | `taskvisor` (+ its `tracing` feature) | Enables the `TracingBridge` re-export       |
//! | `timezone-sync` | off     | `taskvisor`, `solti-model`          | Enables `timezone_sync` periodic task         |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use solti_observe::{LoggerConfig, LoggerLevel, init_local_offset, init_logger};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 1) Must be called before spawning threads (tokio runtime).
//!     init_local_offset();
//!
//!     tokio::runtime::Runtime::new()?.block_on(async {
//!         // 2) Initialize logger
//!         let cfg = LoggerConfig {
//!             level: LoggerLevel::new("info")?,
//!             ..Default::default()
//!         };
//!         init_logger(&cfg)?;
//!
//!         tracing::info!("ready");
//!         Ok(())
//!     })
//! }
//! ```
//!
//! ## Local timezone support
//!
//! On most Unix platforms, detecting the local UTC offset requires reading `/etc/localtime`,
//! which is unsafe in multi-threaded processes.
//!
//! To work around this:
//! 1. Call [`init_local_offset`] in `main()` **before** `tokio::runtime::Runtime::new()`.
//! 2. Optionally submit the `timezone_sync` task to periodically re-detect
//!    the offset (handles DST transitions in long-running daemons).
//!
//! If [`init_local_offset`] is not called, timestamps fall back to UTC with a warning printed to stderr on first use.
//!
//! ## Also
//!
//! - [`tracing`] the underlying structured logging framework.
//! - `taskvisor::Subscribe` the subscriber contract `TracingBridge` implements.
//! - `solti-prometheus` is a complementary metrics subscriber for the same event stream.
//! - See `examples/http-server` for a complete integration example.

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

// Periodic task that re-detects the local UTC offset.
// Enable with: `--features timezone-sync`
#[cfg(feature = "timezone-sync")]
pub use logger::timezone_sync;

// Taskvisor's own tracing bridge, re-exported as the event-logging subscriber.
// It emits structured fields (stable `event` label, `seq`, `id`, `attempt`,
// `reason`, timings) under target "taskvisor" and raises permanent retry
// give-ups to WARN.
#[cfg(feature = "subscriber")]
pub use taskvisor::TracingBridge;
