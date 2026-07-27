//! Validated log level filter.
//!
//! [`LoggerLevel`] wraps a `tracing_subscriber::EnvFilter` string and checks it when config is parsed.

use std::{convert::TryFrom, str::FromStr};

use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use crate::logger::LoggerError;

/// Validated wrapper around a [`tracing_subscriber::EnvFilter`] expression.
///
/// The raw string is preserved. This lets config files round-trip without changing the user's filter expression.
///
/// # Accepted syntax
///
/// Any expression accepted by [`EnvFilter`](tracing_subscriber::EnvFilter) can be used:
///
/// | Expression                                 | Meaning                                          |
/// |--------------------------------------------|--------------------------------------------------|
/// | `"info"`                                   | Global info level                                |
/// | `"debug"`                                  | Global debug level                               |
/// | `"solti_exec=trace,info"`                  | Trace for `solti_exec`, info for everything else |
/// | `"solti_core=debug,solti_exec=trace,warn"` | Per-crate overrides with global fallback         |
///
/// # Example
///
/// ```
/// use solti_observe::LoggerLevel;
///
/// let lvl = LoggerLevel::new("info").unwrap();
/// assert_eq!(lvl.as_str(), "info");
///
/// let lvl: LoggerLevel = "solti_exec=trace,info".parse().unwrap();
/// assert_eq!(lvl.as_str(), "solti_exec=trace,info");
///
/// assert!(LoggerLevel::new("my_crate=lol").is_err());
/// ```
///
/// # Serialization
///
/// It serializes to and from a plain JSON string:
///
/// ```rust
/// # use solti_observe::LoggerLevel;
/// let lvl = LoggerLevel::new("debug").unwrap();
/// let json = serde_json::to_string(&lvl).unwrap();
/// assert_eq!(json, r#""debug""#);
/// ```
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String")]
#[serde(into = "String")]
pub struct LoggerLevel(String);

impl LoggerLevel {
    /// Create a new `LoggerLevel` from a string-like value.
    ///
    /// This is a convenience wrapper around [`TryFrom<String>`].
    ///
    /// # Errors
    ///
    /// - [`LoggerError::InvalidLevel`]: the value is not a valid `EnvFilter` directive string (carries the input and the parse error).
    ///
    /// # Example
    /// ```
    /// use solti_observe::LoggerLevel;
    ///
    /// let lvl = LoggerLevel::new("info").unwrap();
    /// assert_eq!(lvl.as_str(), "info");
    /// ```
    pub fn new(s: impl Into<String>) -> Result<Self, LoggerError> {
        Self::try_from(s.into())
    }

    /// Return the underlying filter string.
    ///
    /// This is exactly what was provided in config, such as `"info"` or `"solti_exec=trace,taskvisor=debug,info"`.
    ///
    /// # Example
    /// ```
    /// use solti_observe::LoggerLevel;
    ///
    /// let lvl = "warn".parse::<LoggerLevel>().unwrap();
    /// assert_eq!(lvl.as_str(), "warn");
    /// ```
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert this value into an environment filter.
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
    fn accepts_valid_levels() {
        let ok = [
            "info",
            "warn",
            "error",
            "trace",
            "debug",
            "solti_exec=trace,solti_core=debug,info",
        ];

        for lvl in ok {
            let parsed = lvl.parse::<LoggerLevel>();
            assert!(
                parsed.is_ok(),
                "expected valid LoggerLevel for {lvl}, got: {parsed:?}"
            );
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
    fn serde_from_plain_string() {
        let json = r#""debug""#;
        let lvl: LoggerLevel = serde_json::from_str(json).unwrap();
        assert_eq!(lvl.as_str(), "debug");
    }

    #[test]
    fn default_is_info_and_valid() {
        let lvl = LoggerLevel::default();
        assert_eq!(lvl.as_str(), "info");

        let _filter = lvl.to_env_filter();
    }

    #[test]
    fn new_matches_parse() {
        let a = LoggerLevel::new("warn").expect("valid level via new()");
        let b: LoggerLevel = "warn".parse().expect("valid level via FromStr");

        assert_eq!(a.as_str(), b.as_str());
    }

    #[test]
    fn to_env_filter_never_panics_for_valid_input() {
        let levels = [
            "info",
            "warn",
            "error",
            "trace",
            "debug",
            "my_crate=trace,info",
        ];

        for level_str in levels {
            let lvl = level_str.parse::<LoggerLevel>().unwrap();
            let _filter = lvl.to_env_filter();
        }
    }
}
