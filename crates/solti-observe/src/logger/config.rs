use serde::{Deserialize, Serialize};
use std::io::IsTerminal;

use crate::logger::object::{LoggerFormat, LoggerLevel, LoggerTimeZone};

/// Logger configuration passed to [`crate::init_logger`].
///
/// Missing fields use [`Default`].
///
/// # Example
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
///
/// # Defaults
///
/// | Field          | Default | Description                                |
/// |----------------|---------|--------------------------------------------|
/// | `format`       | `Text`  | Human-readable colored output              |
/// | `level`        | `info`  | `tracing_subscriber::EnvFilter` expression |
/// | `timezone`     | `Utc`   | Timestamp timezone                         |
/// | `with_targets` | `true`  | Include module/target names in output      |
/// | `use_color`    | `true`  | Colored output (auto-disabled if not TTY)  |
///
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggerConfig {
    /// Output format: text, JSON, or journald with feature `journald`.
    pub format: LoggerFormat,
    /// Log level filter expression, such as `"info"` or `"solti_exec=trace,info"`.
    ///
    /// Validated on construction: see [`LoggerLevel`] for syntax.
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
    /// Return whether text logs should use ANSI colors.
    ///
    /// Color is enabled only when `use_color` is `true` and stdout is a terminal.
    /// JSON logs ignore colors.
    ///
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
    fn serde_uses_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let config: LoggerConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.level.as_str(), LoggerLevel::default().as_str());
        assert_eq!(config.format, LoggerFormat::default());
        assert_eq!(config.timezone, LoggerTimeZone::default());
        assert!(config.with_targets);
        assert!(config.use_color);
    }

    #[test]
    fn partial_deserialization() {
        let json = r#"{"format": "json", "level": "debug"}"#;
        let config: LoggerConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.format, LoggerFormat::Json);
        assert_eq!(config.level.as_str(), "debug");
        assert!(config.with_targets);
        assert!(config.use_color);
    }
}
