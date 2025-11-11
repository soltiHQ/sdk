use time::{UtcOffset, format_description::well_known::Rfc3339};
use tracing::Subscriber;
use tracing_subscriber::{
    EnvFilter, fmt, fmt::time::OffsetTime, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::logger::{config::LoggerConfig, error::LoggerError};

pub struct Logger;

impl Logger {
    pub fn text(cfg: &LoggerConfig) -> Result<(), LoggerError> {
        let filter = mk_filter(&cfg.level)?;
        let fmt_layer = fmt::layer()
            .with_ansi(cfg.use_color)
            .with_target(cfg.with_targets)
            .with_timer(mk_timer());

        let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
        init_with(subscriber)
    }

    pub fn json(cfg: &LoggerConfig) -> Result<(), LoggerError> {
        let filter = mk_filter(&cfg.level)?;
        let fmt_layer = fmt::layer()
            .json()
            .with_ansi(false)
            .with_target(cfg.with_targets)
            .with_timer(mk_timer());

        let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
        init_with(subscriber)
    }

    pub fn journald(cfg: &LoggerConfig) -> Result<(), LoggerError> {
        let filter = mk_filter(&cfg.level)?;
        mk_journald(filter)
    }
}

fn mk_filter(level: &str) -> Result<EnvFilter, LoggerError> {
    EnvFilter::try_new(level).map_err(|_| LoggerError::InvalidLogLevel(level.to_string()))
}

fn mk_timer() -> OffsetTime<Rfc3339> {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    OffsetTime::new(offset, Rfc3339)
}

fn as_error(e: impl std::fmt::Display) -> LoggerError {
    let s = e.to_string();
    if s.contains("SetGlobalDefaultError") {
        LoggerError::AlreadyInitialized
    } else {
        LoggerError::InitializationFailed(s)
    }
}

fn init_with<S>(subscriber: S) -> Result<(), LoggerError>
where
    S: Subscriber + Send + Sync + 'static,
{
    subscriber.try_init().map_err(as_error)
}

#[cfg(all(target_os = "linux", feature = "journald"))]
fn mk_journald(filter: EnvFilter) -> Result<(), LoggerError> {
    let journald = tracing_journald::layer()
        .map_err(|e| LoggerError::InitializationFailed(format!("journald: {e}")))?;
    let subscriber = tracing_subscriber::registry().with(filter).with(journald);
    init_with(subscriber)
}

#[cfg(not(all(target_os = "linux", feature = "journald")))]
fn mk_journald(_filter: EnvFilter) -> Result<(), LoggerError> {
    Err(LoggerError::JournaldNotSupported)
}
