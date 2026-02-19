use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoggerError {
    #[error("Invalid log format: {0} (expected: text|json|journald)")]
    InvalidFormat(String),

    #[error("Journald is not supported on this platform")]
    JournaldNotSupported,

    #[error("Failed to initialize journald: {0}")]
    JournaldInitFailed(String),

    #[error("Logger already initialized")]
    AlreadyInitialized,

    #[error("Invalid timezone: {0}")]
    InvalidTimeZone(String),

    #[error("Failed to initialize local timezone")]
    LocalTimezoneInitFailed,

    #[error("Invalid log level: {0}")]
    InvalidLevel(String),
}

pub type LoggerResult<T> = Result<T, LoggerError>;
