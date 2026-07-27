//! Logger initialization and configuration.
//!
//! The public entry point is [`init_logger`]. It installs one global tracing subscriber for the process.

mod config;
pub use config::LoggerConfig;

mod error;
pub use error::LoggerError;

mod log;
pub use object::LoggerFormat;

mod object;
pub use object::LoggerLevel;
pub use object::LoggerTimeZone;

mod tasks;

#[cfg(feature = "timezone-sync")]
pub use tasks::timezone_sync;

/// Install the global tracing subscriber.
///
/// The selected backend comes from [`LoggerConfig::format`]:
///
/// | Format                    | Backend                         | Notes                        |
/// |---------------------------|---------------------------------|------------------------------|
/// | [`LoggerFormat::Text`]    | `tracing_subscriber::fmt`       | Colored, human-readable      |
/// | [`LoggerFormat::Json`]    | `tracing_subscriber::fmt::json` | Structured, machine-readable |
/// | `Journald`                | `tracing_journald`              | Feature `journald`, Linux    |
///
/// After this call, all `tracing` macros use this config.
/// The function can succeed only once per process.
/// A later call returns an initialization error.
///
/// # Local timezone
///
/// Local offset detection happens before the global subscriber is installed.
/// Failure is returned as [`LoggerError::LocalOffsetUnavailable`].
///
/// # Example
///
/// ```rust,no_run
/// use solti_observe::{LoggerConfig, LoggerLevel, init_logger};
///
/// # fn main() -> Result<(), solti_observe::LoggerError> {
/// let config = LoggerConfig {
///     level: LoggerLevel::new("taskvisor=debug,info")?,
///     ..Default::default()
/// };
///
/// init_logger(&config)?;
/// tracing::info!("logger ready");
/// # Ok(()) }
/// ```
pub fn init_logger(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    match cfg.format {
        LoggerFormat::Text => log::logger_text(cfg),
        LoggerFormat::Json => log::logger_json(cfg),
        #[cfg(feature = "journald")]
        LoggerFormat::Journald => log::logger_journald(cfg),
    }
}
