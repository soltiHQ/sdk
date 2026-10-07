use thiserror::Error;

/// Invalid calendar configuration or an occurrence search failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CronError {
    /// The expression cannot be parsed as five or six cron fields.
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    /// The timezone is absent from the bundled IANA database.
    #[error("unknown IANA timezone: {0}")]
    InvalidTimezone(String),
    /// The parser cannot find a future occurrence within its calendar range.
    #[error("no future cron occurrence: {0}")]
    NoOccurrence(#[source] croner::errors::CronError),
    /// A duration is zero or cannot be represented by a runtime deadline.
    #[error("{0} must be positive and representable as a runtime deadline")]
    InvalidDuration(&'static str),
}
