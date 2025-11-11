mod config;
mod error;
mod format;
mod log;

pub use config::LoggerConfig;
pub use error::LoggerError;
pub use format::LoggerFormat;

pub fn logger_init(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    match cfg.format {
        LoggerFormat::Text => log::Logger::text(cfg),
        LoggerFormat::Json => log::Logger::json(cfg),
        LoggerFormat::Journald => log::Logger::journald(cfg),
    }
}
