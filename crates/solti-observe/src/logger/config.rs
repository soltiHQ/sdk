//! # Logger configuration
//!
//! [`LoggerConfig`] contains every setting used during logger installation.
//! Serde fills omitted fields from [`Default`] and rejects unknown fields.
//!
//! ```text
//! serialized config ──► LoggerConfig ──► init_logger
//! ```

use serde::{Deserialize, Serialize};
use std::io::IsTerminal;

use crate::logger::object::{LoggerFormat, LoggerLevel, LoggerTimeZone};

/// Complete settings passed to [`crate::init_logger`].
///
/// ## Defaults
///
/// | Field          | Default | Used by                                |
/// |----------------|---------|----------------------------------------|
/// | `format`       | `Text`  | Backend selection                      |
/// | `level`        | `info`  | Every backend                          |
/// | `timezone`     | `Utc`   | Text and JSON timestamps               |
/// | `with_targets` | `true`  | Text and JSON event targets            |
/// | `use_color`    | `true`  | Text output on an interactive terminal |
///
/// Missing Serde fields use these defaults. Unknown fields are rejected so a
/// misspelled setting cannot silently retain its default value.
///
/// ## Example
///
/// ```rust
/// use solti_observe::{LoggerConfig, LoggerFormat};
///
/// let cfg: LoggerConfig = serde_json::from_str("{}").unwrap();
/// assert_eq!(cfg.format, LoggerFormat::Text);
///
/// let cfg: LoggerConfig = serde_json::from_str(r#"{"level":"debug"}"#).unwrap();
/// assert_eq!(cfg.level.as_str(), "debug");
/// ```
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggerConfig {
    /// Output backend.
    pub format: LoggerFormat,
    /// Validated event filter.
    ///
    /// See [`LoggerLevel`] for accepted expressions.
    pub level: LoggerLevel,
    /// Timestamp timezone for text and JSON logs.
    pub timezone: LoggerTimeZone,
    /// Whether text and JSON logs include the event target.
    pub with_targets: bool,
    /// Whether to use colored output.
    ///
    /// Colors are used only for text written to an interactive terminal.
    pub use_color: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            format: LoggerFormat::default(),
            level: LoggerLevel::default(),
            timezone: LoggerTimeZone::default(),
            with_targets: true,
            use_color: true,
        }
    }
}

impl LoggerConfig {
    /// Returns whether text output should use ANSI colors.
    ///
    /// Color is enabled only when `use_color` is `true` and stdout is a terminal.
    /// JSON logs ignore colors.
    pub(crate) fn should_use_color(&self) -> bool {
        self.use_color && std::io::stdout().is_terminal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let config = LoggerConfig::default();

        assert_eq!(config.format, LoggerFormat::Text);
        assert_eq!(config.timezone, LoggerTimeZone::Utc);
        assert_eq!(config.level.as_str(), "info");
        assert!(config.with_targets);
        assert!(config.use_color);
    }

    #[test]
    fn serde_roundtrip() {
        let config = LoggerConfig {
            format: LoggerFormat::Json,
            timezone: LoggerTimeZone::Local,
            level: "debug".parse().unwrap(),
            with_targets: false,
            use_color: false,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: LoggerConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config.level.as_str(), parsed.level.as_str());
        assert_eq!(config.with_targets, parsed.with_targets);
        assert_eq!(config.use_color, parsed.use_color);
        assert_eq!(config.format, parsed.format);
        assert_eq!(config.timezone, parsed.timezone);
    }

    #[test]
    fn serde_applies_defaults_to_missing_fields() {
        let json = r#"{"format": "json", "level": "debug"}"#;
        let config: LoggerConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.format, LoggerFormat::Json);
        assert_eq!(config.level.as_str(), "debug");
        assert_eq!(config.timezone, LoggerTimeZone::Utc);
        assert!(config.with_targets);
        assert!(config.use_color);
    }

    #[test]
    fn serde_rejects_unknown_fields() {
        let error = serde_json::from_str::<LoggerConfig>(r#"{"levle": "debug"}"#)
            .expect_err("an unknown logger setting must be rejected");
        let message = error.to_string();

        assert!(message.contains("unknown field `levle`"), "{message}");
        assert!(message.contains("`level`"), "{message}");
    }
}
