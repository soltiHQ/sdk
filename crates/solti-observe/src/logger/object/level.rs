use std::{convert::TryFrom, str::FromStr};

use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use crate::logger::LoggerError;

/// Wrapper around a `tracing_subscriber::EnvFilter` expression.
///
/// This type is used at the configuration layer:
/// - It stores the raw filter string (e.g. `"info"`, `"solti_exec=trace,solti_core=debug,info"`).
/// - It validates the value using `EnvFilter::try_new` when parsing from config / user input.
/// - It can be converted into an actual `EnvFilter` on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String")]
#[serde(into = "String")]
pub struct LoggerLevel(String);

impl LoggerLevel {
    /// Creates a new `LoggerLevel` from a string-like value.
    ///
    /// This is a convenience wrapper around [`TryFrom<String>`].
    ///
    /// # Examples
    /// ```
    /// use solti_observe::LoggerLevel;
    ///
    /// let lvl = LoggerLevel::new("info").unwrap();
    /// assert_eq!(lvl.as_str(), "info");
    /// ```
    pub fn new(s: impl Into<String>) -> Result<Self, LoggerError> {
        Self::try_from(s.into())
    }

    /// Returns the underlying filter string as `&str`.
    ///
    /// This is exactly what was provided in config
    /// (e.g. `"info"` or `"solti_exec=trace,taskvisor=debug,info"`).
    ///
    /// # Examples
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

    /// Parses the underlying string into a `tracing_subscriber::EnvFilter`.
    ///
    /// # Examples
    /// ```
    /// use solti_observe::LoggerLevel;
    ///
    /// let lvl = "solti_exec=trace,info".parse::<LoggerLevel>().unwrap();
    /// let _ = lvl.to_env_filter();
    /// ```
    pub fn to_env_filter(&self) -> EnvFilter {
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
    fn try_from(s: String) -> Result<Self, Self::Error> {
        match EnvFilter::try_new(&s) {
            Ok(_) => Ok(LoggerLevel(s)),
            Err(e) => Err(LoggerError::InvalidLevel(format!("{}: {}", s, e))),
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
    use serde_json;
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
