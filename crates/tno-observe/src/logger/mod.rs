mod config;
mod error;
mod logger;
mod object;
mod tasks;

pub use config::LoggerConfig;
pub use error::LoggerError;
pub use object::LoggerFormat;
pub use object::LoggerLevel;
pub use object::{LoggerTimeZone, init_local_offset};

#[cfg(feature = "timezone-sync")]
pub use tasks::timezone_sync;

/// Initializes the global tracing subscriber with the given configuration.
///
/// This function configures and installs a tracing subscriber based on the provided [`LoggerConfig`].
/// Once initialized, all `tracing` macros (`info!`, `debug!`, etc.) will use this configuration.
///
/// # Important: Local Timezone
/// For using `LoggerTimeZone::Local`, you **must** call [`object::timezone::init_local_offset`]
/// in `main()` function before spawning any threads.
///
/// # Examples
/// ```rust
/// use tno_observe::{LoggerConfig, init_logger};
///
/// fn main() {
///     let config = LoggerConfig::default();
///     init_logger(&config).expect("Failed to initialize logger");
///
///     tracing::info!("Logger initialized successfully");
/// }
/// ```
pub fn init_logger(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    match cfg.format {
        LoggerFormat::Text => logger::logger_text(cfg),
        LoggerFormat::Json => logger::logger_json(cfg),
        LoggerFormat::Journald => logger::logger_journald(cfg),
    }
}
