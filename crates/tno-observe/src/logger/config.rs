use serde::{Deserialize, Serialize};
use std::io::IsTerminal;

use crate::logger::object::{LoggerFormat, LoggerLevel, LoggerTimeZone};

/// Logger configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggerConfig {
    /// Output format.
    pub format: LoggerFormat,
    /// Log level filter expression (e.g., "info", "my_crate=debug,info").
    pub level: LoggerLevel,
    /// Timezone for timestamps.
    pub tz: LoggerTimeZone,
    /// Whether to include module/target names in log output.
    pub with_targets: bool,
    /// Whether to use colored output.
    pub use_color: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            format: LoggerFormat::default(),
            level: LoggerLevel::default(),
            tz: LoggerTimeZone::default(),
            with_targets: true,
            use_color: true,
        }
    }
}

impl LoggerConfig {
    /// Determines whether colored output should be used.
    ///
    /// Color is enabled only if:
    /// 1. `use_color` config is `true` (user hasn't explicitly disabled it), AND
    /// 2. stdout is a terminal (not redirected to a file/pipe)
    ///
    /// This method should be called during logger initialization, not during
    /// config parsing, to ensure accurate terminal detection.
    ///
    /// # Examples
    /// ```rust
    /// use tno_observe::LoggerConfig;
    ///
    /// let config = LoggerConfig::default();
    /// let should_use_color = config.should_use_color();
    /// // Returns true only if stdout is currently a terminal
    /// ```
    pub fn should_use_color(&self) -> bool {
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
        assert_eq!(config.tz, LoggerTimeZone::Utc);
        assert_eq!(config.level.as_str(), "info");
        assert_eq!(config.with_targets, true);
        assert_eq!(config.use_color, true);
    }

    #[test]
    fn serde_roundtrip() {
        let config = LoggerConfig {
            format: LoggerFormat::Json,
            tz: LoggerTimeZone::Local,
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
        assert_eq!(config.tz, parsed.tz);
    }

    #[test]
    fn serde_uses_defaults_for_missing_fields() {
        let json = r#"{}"#;
        let config: LoggerConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.level.as_str(), LoggerLevel::default().as_str());
        assert_eq!(config.format, LoggerFormat::default());
        assert_eq!(config.tz, LoggerTimeZone::default());
        assert_eq!(config.with_targets, true);
        assert_eq!(config.use_color, true);
    }

    #[test]
    fn partial_deserialization() {
        let json = r#"{"format": "json", "level": "debug"}"#;
        let config: LoggerConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.format, LoggerFormat::Json);
        assert_eq!(config.level.as_str(), "debug");
        assert_eq!(config.with_targets, true);
        assert_eq!(config.use_color, true);
    }
}
