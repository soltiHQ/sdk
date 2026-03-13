mod config;
pub use config::LoggerConfig;

mod error;
pub use error::LoggerError;

mod log;
pub use object::LoggerFormat;

mod object;
pub use object::LoggerLevel;
pub use object::{LoggerTimeZone, init_local_offset};

mod tasks;

// Periodic task that re-detects the local UTC offset.
// Enable with: `--features timezone-sync`
#[cfg(feature = "timezone-sync")]
pub use tasks::timezone_sync;

/// Initializes the global tracing subscriber based on the given [`LoggerConfig`].
///
/// Dispatches to the appropriate backend based on [`LoggerConfig::format`]:
///
/// | Format                    | Backend                         | Notes                        |
/// |---------------------------|---------------------------------|------------------------------|
/// | [`LoggerFormat::Text`]    | `tracing_subscriber::fmt`       | Colored, human-readable      |
/// | [`LoggerFormat::Json`]    | `tracing_subscriber::fmt::json` | Structured, machine-readable |
/// | [`LoggerFormat::Journald`]| `tracing_journald`              | Linux only                   |
///
/// Once initialized, all `tracing` macros (`info!`, `debug!`, etc.) will use this configuration.
/// Can only be called **once** per process - subsequent calls return [`LoggerError::AlreadyInitialized`].
///
/// ## Local timezone
///
/// When using [`LoggerTimeZone::Local`], call [`init_local_offset`] in `main()` **before** spawning the tokio runtime.
/// See the [crate-level docs](crate#local-timezone-support) for details.
///
/// ## Examples
///
/// ```rust
/// use solti_observe::{LoggerConfig, init_logger};
///
/// let config = LoggerConfig::default();
/// init_logger(&config).expect("Failed to initialize logger");
///
/// tracing::info!("Logger initialized successfully");
/// ```
pub fn init_logger(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    match cfg.format {
        LoggerFormat::Text => log::logger_text(cfg),
        LoggerFormat::Json => log::logger_json(cfg),
        LoggerFormat::Journald => log::logger_journald(cfg),
    }
}
