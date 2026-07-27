//! Logger backend dispatch.
//!
//! Builds and installs the global tracing subscriber for each [`LoggerFormat`](crate::LoggerFormat).

use tracing::Subscriber;
#[cfg(feature = "log-compat")]
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, layer::SubscriberExt};

use crate::logger::{
    config::LoggerConfig,
    error::LoggerError,
    object::{LoggerTimeZone, Rfc3339Timer, initialize_local_offset},
};

/// Initialize the text logger.
pub(super) fn logger_text(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    let filter = cfg.level.to_env_filter();
    let timer = timer(cfg)?;
    let fmt_layer = fmt::layer()
        .with_ansi(cfg.should_use_color())
        .with_target(cfg.with_targets)
        .with_timer(timer);

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    init_subscriber(subscriber)
}

/// Initialize the JSON logger.
pub(super) fn logger_json(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    let filter = cfg.level.to_env_filter();
    let timer = timer(cfg)?;
    let fmt_layer = fmt::layer()
        .json()
        .with_ansi(false)
        .with_target(cfg.with_targets)
        .with_timer(timer);

    let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
    init_subscriber(subscriber)
}

/// Initialize the journald logger on Linux.
#[cfg(all(feature = "journald", target_os = "linux"))]
pub(super) fn logger_journald(cfg: &LoggerConfig) -> Result<(), LoggerError> {
    let filter = cfg.level.to_env_filter();
    let journald = tracing_journald::layer().map_err(LoggerError::JournaldInitFailed)?;

    let subscriber = tracing_subscriber::registry().with(filter).with(journald);
    init_subscriber(subscriber)
}

/// Stub for journald on non-Linux platforms.
#[cfg(all(feature = "journald", not(target_os = "linux")))]
pub(super) fn logger_journald(_cfg: &LoggerConfig) -> Result<(), LoggerError> {
    Err(LoggerError::JournaldNotSupported)
}

fn timer(cfg: &LoggerConfig) -> Result<Rfc3339Timer, LoggerError> {
    if cfg.timezone == LoggerTimeZone::Local {
        initialize_local_offset()?;
    }
    Ok(Rfc3339Timer::new(cfg.timezone))
}

/// Installs the subscriber as the global default.
fn init_subscriber<S>(subscriber: S) -> Result<(), LoggerError>
where
    S: Subscriber + Send + Sync + 'static,
{
    #[cfg(feature = "log-compat")]
    {
        subscriber.try_init().map_err(LoggerError::LoggerInitFailed)
    }

    #[cfg(not(feature = "log-compat"))]
    {
        tracing::subscriber::set_global_default(subscriber).map_err(LoggerError::AlreadyInitialized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::object::LoggerFormat;
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn text_config_has_correct_fields() {
        let config = LoggerConfig {
            format: LoggerFormat::Text,
            timezone: LoggerTimeZone::Utc,
            level: "info".parse().unwrap(),
            with_targets: true,
            use_color: false,
        };

        assert_eq!(config.format, LoggerFormat::Text);
        assert_eq!(config.level.as_str(), "info");
    }

    #[test]
    fn json_config_has_correct_fields() {
        let config = LoggerConfig {
            format: LoggerFormat::Json,
            timezone: LoggerTimeZone::Utc,
            level: "debug".parse().unwrap(),
            with_targets: false,
            use_color: true,
        };

        assert_eq!(config.format, LoggerFormat::Json);
        assert_eq!(config.level.as_str(), "debug");
    }

    #[test]
    #[cfg(all(feature = "journald", not(target_os = "linux")))]
    fn init_journald_returns_error_when_not_supported() {
        let config = LoggerConfig {
            format: LoggerFormat::Journald,
            ..Default::default()
        };

        let result = logger_journald(&config);
        assert!(matches!(result, Err(LoggerError::JournaldNotSupported)));
    }

    #[test]
    fn text_output_has_one_timestamp_separator() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(Arc::clone(&output));
        let layer = fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_timer(Rfc3339Timer::new(LoggerTimeZone::Utc))
            .with_writer(move || writer.clone());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || tracing::info!("ready"));

        let rendered = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let level_index = rendered.find("INFO").unwrap();
        let before_level = &rendered[..level_index];
        let separator_width = before_level
            .chars()
            .rev()
            .take_while(|character| character.is_ascii_whitespace())
            .count();
        let timestamp = before_level.trim_end();

        assert!(timestamp.ends_with('Z') || timestamp.ends_with("+00:00"));
        assert_eq!(separator_width, 2, "{rendered:?}");
    }

    #[test]
    fn json_timestamp_has_no_trailing_space() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter(Arc::clone(&output));
        let layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_timer(Rfc3339Timer::new(LoggerTimeZone::Utc))
            .with_writer(move || writer.clone());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || tracing::info!("ready"));

        let rendered = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let event: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let timestamp = event["timestamp"].as_str().unwrap();
        assert_eq!(timestamp.trim(), timestamp);
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
