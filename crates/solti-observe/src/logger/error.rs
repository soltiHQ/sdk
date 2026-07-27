//! Logger error type.

use thiserror::Error;

/// Errors returned while validating logger settings or installing the logging backend.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoggerError {
    /// The format string was not one of `text` / `json` / `journald`.
    #[error("invalid log format: {0}")]
    InvalidFormat(String),

    /// Journald was requested without the `journald` feature.
    #[error("journald support is not enabled")]
    JournaldNotEnabled,

    /// The `journald` format was requested on a non-Linux target, where the systemd journal is unavailable.
    #[cfg(feature = "journald")]
    #[error("journald is not supported on this platform")]
    JournaldNotSupported,

    /// Connecting to the systemd journal failed.
    #[cfg(feature = "journald")]
    #[error("failed to initialize journald")]
    JournaldInitFailed(#[source] std::io::Error),

    /// A global tracing subscriber is already installed.
    #[cfg(not(feature = "log-compat"))]
    #[error("global tracing subscriber is already installed")]
    AlreadyInitialized(#[source] tracing::dispatcher::SetGlobalDefaultError),

    /// The tracing subscriber or `log` compatibility bridge could not be installed.
    #[cfg(feature = "log-compat")]
    #[error("failed to initialize the global logger")]
    LoggerInitFailed(#[source] tracing_subscriber::util::TryInitError),

    /// The timezone string was not `utc` or `local`.
    #[error("invalid timezone: {0}")]
    InvalidTimeZone(String),

    /// The level/`EnvFilter` expression failed to parse.
    #[error("invalid log level {value:?}")]
    InvalidLevel {
        /// Invalid filter expression.
        value: String,
        /// Parser error.
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },

    /// The local UTC offset could not be determined.
    #[error("failed to determine the local UTC offset")]
    LocalOffsetUnavailable(#[source] time::error::IndeterminateOffset),
}
