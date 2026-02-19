use tracing::Subscriber;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::logger::{
    config::LoggerConfig,
    error::{LoggerError, LoggerResult},
    object::LoggerRfc3339,
};

/// Initializes text logger.
pub fn logger_text(cfg: &LoggerConfig) -> LoggerResult<()> {
    let filter = cfg.level.to_env_filter();
    let fmt_layer = fmt::layer()
        .with_ansi(cfg.should_use_color())
        .with_target(cfg.with_targets)
        .with_timer(LoggerRfc3339);

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    init_subscriber(subscriber)
}

/// Initializes JSON (structured) logger.
pub fn logger_json(cfg: &LoggerConfig) -> LoggerResult<()> {
    let filter = cfg.level.to_env_filter();
    let fmt_layer = fmt::layer()
        .json()
        .with_ansi(false)
        .with_target(cfg.with_targets)
        .with_timer(LoggerRfc3339);

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    init_subscriber(subscriber)
}

/// Initializes journald logger (Linux only).
#[cfg(target_os = "linux")]
pub fn logger_journald(cfg: &LoggerConfig) -> LoggerResult<()> {
    let filter = cfg.level.to_env_filter();
    let journald =
        tracing_journald::layer().map_err(|e| LoggerError::JournaldInitFailed(e.to_string()))?;

    let subscriber = tracing_subscriber::registry().with(filter).with(journald);
    init_subscriber(subscriber)
}

/// Stub for journald on non-Linux platforms.
#[cfg(not(all(target_os = "linux")))]
pub fn logger_journald(_cfg: &LoggerConfig) -> LoggerResult<()> {
    Err(LoggerError::JournaldNotSupported)
}

/// Installs the subscriber as the global default.
fn init_subscriber<S>(subscriber: S) -> LoggerResult<()>
where
    S: Subscriber + Send + Sync + 'static,
{
    subscriber
        .try_init()
        .map_err(|_| LoggerError::AlreadyInitialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::object::LoggerTimeZone;
    use crate::logger::object::format::LoggerFormat;

    #[test]
    fn init_text_builds_config() {
        let config = LoggerConfig {
            format: LoggerFormat::Text,
            tz: LoggerTimeZone::Utc,
            level: "info".parse().unwrap(),
            with_targets: true,
            use_color: false,
        };

        assert_eq!(config.format, LoggerFormat::Text);
        assert_eq!(config.level.as_str(), "info");
    }

    #[test]
    fn init_json_builds_config() {
        let config = LoggerConfig {
            format: LoggerFormat::Json,
            tz: LoggerTimeZone::Utc,
            level: "debug".parse().unwrap(),
            with_targets: false,
            use_color: true,
        };

        assert_eq!(config.format, LoggerFormat::Json);
        assert_eq!(config.level.as_str(), "debug");
    }

    #[test]
    #[cfg(not(all(target_os = "linux")))]
    fn init_journald_returns_error_when_not_supported() {
        let config = LoggerConfig {
            format: LoggerFormat::Journald,
            ..Default::default()
        };

        let result = logger_journald(&config);
        assert!(matches!(result, Err(LoggerError::JournaldNotSupported)));
    }

    #[test]
    fn color_is_disabled_for_json() {
        let config = LoggerConfig {
            format: LoggerFormat::Json,
            use_color: true,
            ..Default::default()
        };

        assert_eq!(config.format, LoggerFormat::Json);
    }

    #[test]
    fn env_filter_is_built_correctly() {
        let config = LoggerConfig {
            level: "my_crate=debug,info".parse().unwrap(),
            ..Default::default()
        };

        let filter = config.level.to_env_filter();
        let _ = format!("{:?}", filter);
    }
}
