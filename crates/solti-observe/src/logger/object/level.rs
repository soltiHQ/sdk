//! # Log filtering
//!
//! [`LoggerLevel`] validates a `tracing_subscriber::EnvFilter` expression.
//! It keeps invalid filters out of [`LoggerConfig`](crate::LoggerConfig).
//!
//! ```text
//! filter string ──► LoggerLevel ──► EnvFilter ──► subscriber
//! ```

use std::{convert::TryFrom, str::FromStr};

use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use crate::logger::LoggerError;

/// Validated [`tracing_subscriber::EnvFilter`] expression.
///
/// The original string is preserved.
/// Serialization does not rewrite the expression.
///
/// ## Syntax
///
/// Any expression accepted by [`EnvFilter`](tracing_subscriber::EnvFilter) can be used.
///
/// | Expression                                 | Meaning                                          |
/// |--------------------------------------------|--------------------------------------------------|
/// | `"info"`                                   | Global info level                                |
/// | `"debug"`                                  | Global debug level                               |
/// | `"solti_exec=trace,info"`                  | Trace for `solti_exec`, info for everything else |
/// | `"solti_core=debug,solti_exec=trace,warn"` | Per-crate overrides with global fallback         |
///
/// ## Serialization
///
/// Serde uses one plain string.
///
/// ## Example
///
/// ```
/// use solti_observe::LoggerLevel;
///
/// let level = LoggerLevel::new("solti_exec=trace,info").unwrap();
/// assert_eq!(level.as_str(), "solti_exec=trace,info");
///
/// assert!(LoggerLevel::new("my_crate=lol").is_err());
/// assert_eq!(serde_json::to_string(&level).unwrap(), r#""solti_exec=trace,info""#);
/// ```
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String")]
#[serde(into = "String")]
pub struct LoggerLevel(String);

impl LoggerLevel {
    /// Creates a validated filter from a string-like value.
    ///
    /// # Errors
    ///
    /// Returns [`LoggerError::InvalidLevel`] when `EnvFilter` rejects the expression.
    pub fn new(s: impl Into<String>) -> Result<Self, LoggerError> {
        Self::try_from(s.into())
    }

    /// Returns the original filter string.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Builds the validated environment filter.
    pub(crate) fn to_env_filter(&self) -> EnvFilter {
        EnvFilter::try_new(self.as_str()).expect("LoggerLevel is always valid after construction")
    }
}

impl Default for LoggerLevel {
    fn default() -> Self {
        Self::try_from("info".to_string()).expect("default log level must be valid")
    }
}

impl FromStr for LoggerLevel {
    type Err = LoggerError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_owned())
    }
}

impl TryFrom<String> for LoggerLevel {
    type Error = LoggerError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        match EnvFilter::try_new(&value) {
            Ok(_) => Ok(LoggerLevel(value)),
            Err(parse_error) => Err(LoggerError::InvalidLevel {
                value,
                source: parse_error,
            }),
        }
    }
}

impl From<LoggerLevel> for String {
    fn from(l: LoggerLevel) -> Self {
        l.0
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::LoggerLevel;

    #[test]
    fn valid_levels_build_filters_through_both_constructors() {
        let ok = [
            "info",
            "warn",
            "error",
            "trace",
            "debug",
            "solti_exec=trace,solti_core=debug,info",
        ];

        for lvl in ok {
            let parsed = lvl.parse::<LoggerLevel>().unwrap();
            let constructed = LoggerLevel::new(lvl).unwrap();
            assert_eq!(parsed, constructed);
            let _filter = parsed.to_env_filter();
        }
    }

    #[test]
    fn rejects_invalid_levels() {
        let bad = [
            "my_crate=lol",
            "solti_exec=verbose",
            "other=trace,another=wat",
            "root=info,subcrate=xyz",
        ];

        for lvl in bad {
            let parsed = LoggerLevel::from_str(lvl);
            assert!(
                parsed.is_err(),
                "expected error for invalid LoggerLevel {lvl}, but got Ok",
            );
        }
    }

    #[test]
    fn serde_roundtrip() {
        let original: LoggerLevel = "solti_exec=trace,info"
            .parse()
            .expect("valid filter must parse");

        let json = serde_json::to_string(&original).expect("LoggerLevel must serialize to JSON");
        let restored: LoggerLevel =
            serde_json::from_str(&json).expect("LoggerLevel must deserialize from JSON");

        assert_eq!(
            original.as_str(),
            restored.as_str(),
            "serde roundtrip should preserve underlying string"
        );
    }

    #[test]
    fn default_is_info_and_valid() {
        let lvl = LoggerLevel::default();
        assert_eq!(lvl.as_str(), "info");

        let _filter = lvl.to_env_filter();
    }
}
