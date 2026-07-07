//! # Logger error type.

use thiserror::Error;

/// Errors returned by logger setup and by config value-object parsing.
///
/// The parsing variants (`InvalidFormat`, `InvalidTimeZone`, `InvalidLevel`) come from
/// `FromStr` and serde construction of [`LoggerConfig`](crate::LoggerConfig) fields.
/// The remaining variants come from [`init_logger`](crate::init_logger).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoggerError {
    /// The format string was not one of `text` / `json` / `journald`.
    #[error("Invalid log format: {0} (expected: text|json|journald)")]
    InvalidFormat(String),

    /// The `journald` format was requested on a non-Linux target, where the systemd journal is unavailable.
    #[error("Journald is not supported on this platform")]
    JournaldNotSupported,

    /// Connecting to the systemd journal failed (carries the underlying error).
    #[error("Failed to initialize journald: {0}")]
    JournaldInitFailed(String),

    /// A global tracing subscriber is already installed.
    ///
    /// [`init_logger`](crate::init_logger) can succeed at most once per process.
    #[error("Logger already initialized")]
    AlreadyInitialized,

    /// The timezone string was not `utc` or `local`.
    #[error("Invalid timezone: {0}")]
    InvalidTimeZone(String),

    /// The level/`EnvFilter` expression failed to parse.
    #[error("Invalid log level: {0}")]
    InvalidLevel(String),
}
