//! # Logger initialization
//!
//! [`init_logger`] installs one global subscriber from [`LoggerConfig`].
//!
//! ```text
//! LoggerConfig ──► backend selection ──► global tracing subscriber
//! ```

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

/// Installs the global tracing subscriber.
///
/// The selected backend comes from [`LoggerConfig::format`]:
///
/// | Format                 | Backend                         | Configuration used               |
/// |------------------------|---------------------------------|----------------------------------|
/// | [`LoggerFormat::Text`] | `tracing_subscriber::fmt`       | Level, timezone, targets, colors |
/// | [`LoggerFormat::Json`] | `tracing_subscriber::fmt::json` | Level, timezone, targets         |
/// | `Journald`             | `tracing_journald`              | Level                            |
///
/// The function can succeed only once for the process-global subscriber.
/// A later global installation returns an initialization error.
///
/// ## Local Time
///
/// Text and JSON initialize the cached local offset for [`LoggerTimeZone::Local`].
/// Detection happens before the global subscriber is installed.
/// Journald does not use this setting.
///
/// The detected offset stays in a process-global cache.
/// Feature `timezone-sync` provides the refresh task.
///
/// ## Example
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
///
/// # Errors
///
/// Returns [`LoggerError`] when backend setup fails.
/// This includes local offset detection, journald connection, and global installation.
pub fn init_logger(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    match cfg.format {
        LoggerFormat::Text => log::logger_text(cfg),
        LoggerFormat::Json => log::logger_json(cfg),
        #[cfg(feature = "journald")]
        LoggerFormat::Journald => log::logger_journald(cfg),
    }
}
