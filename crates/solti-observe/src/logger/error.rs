//! # Logger errors
//!
//! [`LoggerError`] reports invalid settings and backend installation failures.

use thiserror::Error;

/// Error returned while validating settings or installing a logger.
///
/// Match with a wildcard arm because this enum is non-exhaustive.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LoggerError {
    /// The format string was not recognized.
    #[error("invalid log format: {0}")]
    InvalidFormat(String),

    /// Journald was requested without the `journald` feature.
    #[error("journald support is not enabled")]
    JournaldNotEnabled,

    /// Journald was requested on a non-Linux target.
    #[cfg(feature = "journald")]
    #[cfg_attr(docsrs, doc(cfg(feature = "journald")))]
    #[error("journald is not supported on this platform")]
    JournaldNotSupported,

    /// Connecting to the systemd journal failed.
    #[cfg(feature = "journald")]
    #[cfg_attr(docsrs, doc(cfg(feature = "journald")))]
    #[error("failed to initialize journald")]
    JournaldInitFailed(#[source] std::io::Error),

    /// A global tracing subscriber is already installed.
    #[cfg(not(feature = "log-compat"))]
    #[error("global tracing subscriber is already installed")]
    AlreadyInitialized(#[source] tracing::dispatcher::SetGlobalDefaultError),

    /// The tracing subscriber or `log` compatibility bridge could not be installed.
    #[cfg(feature = "log-compat")]
    #[cfg_attr(docsrs, doc(cfg(feature = "log-compat")))]
    #[error("failed to initialize the global logger")]
    LoggerInitFailed(#[source] tracing_subscriber::util::TryInitError),

    /// The timezone string was not `utc` or `local`.
    #[error("invalid timezone: {0}")]
    InvalidTimeZone(String),

    /// The `EnvFilter` expression failed to parse.
    #[error("invalid log level {value:?}")]
    InvalidLevel {
        /// Original filter expression.
        value: String,
        /// Filter parser error.
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },

    /// The current local UTC offset could not be determined.
    #[error("failed to determine the local UTC offset")]
    LocalOffsetUnavailable(#[source] time::error::IndeterminateOffset),
}
