use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoggerError {
    #[error("Invalid logger format: {0} (expected: text|json|journald)")]
    InvalidFormat(String),
    #[error("Journald is not supported on this platform or feature disabled")]
    JournaldNotSupported,
    #[error("Logger has been already initialized")]
    AlreadyInitialized,
    #[error("Failed to initialize logger: {0}")]
    InitializationFailed(String),
    #[error("Invalid log level: {0}")]
    InvalidLogLevel(String),
}
