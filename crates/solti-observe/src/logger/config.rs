use serde::{Deserialize, Serialize};
use std::io::IsTerminal;

use crate::logger::object::{LoggerFormat, LoggerLevel, LoggerTimeZone};

/// Logger configuration passed to [`crate::init_logger`].
///
/// ```rust
/// use solti_observe::LoggerConfig;
///
/// // Empty JSON → all defaults
/// let cfg: LoggerConfig = serde_json::from_str("{}").unwrap();
/// assert_eq!(cfg.format, solti_observe::LoggerFormat::Text);
///
/// // Override only what you need
/// let cfg: LoggerConfig = serde_json::from_str(r#"{"level":"debug"}"#).unwrap();
/// assert_eq!(cfg.level.as_str(), "debug");
/// ```
///
/// ## Defaults
///
/// | Field          | Default | Description                                |
/// |----------------|---------|--------------------------------------------|
/// | `format`       | `Text`  | Human-readable colored output              |
/// | `level`        | `info`  | `tracing_subscriber::EnvFilter` expression |
/// | `tz`           | `Utc`   | Timestamp timezone                         |
/// | `with_targets` | `true`  | Include module/target names in output      |
/// | `use_color`    | `true`  | Colored output (auto-disabled if not TTY)  |
///
/// ## Also
///
/// - [`init_logger`](crate::init_logger) consumes this config to install the global subscriber.
/// - [`LoggerTimeZone`] timezone variants and the `init_local_offset` requirement.
/// - [`LoggerFormat`] output format variants and their backends.
/// - [`LoggerLevel`] validated filter expression syntax.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggerConfig {
    /// Output format: [`LoggerFormat::Text`], [`LoggerFormat::Json`], or [`LoggerFormat::Journald`].
    pub format: LoggerFormat,
    /// Log level filter expression (e.g. `"info"`, `"solti_exec=trace,info"`).
    ///
    /// Validated on construction: see [`LoggerLevel`] for syntax.
    pub level: LoggerLevel,
    /// Timestamp timezone: [`LoggerTimeZone::Utc`] or [`LoggerTimeZone::Local`].
    pub tz: LoggerTimeZone,
    /// Whether to include module/target names in log output.
    pub with_targets: bool,
    /// Whether to use colored output.
    ///
    /// Actual color usage also depends on stdout being a terminal - see [`should_use_color`](Self::should_use_color).
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
    /// `use_color` config is `true` AND stdout is a terminal.
    ///
    /// This method should be called during logger initialization, not during
    /// config parsing, to ensure accurate terminal detection.
    ///
    /// # Examples
    /// ```rust
    /// use solti_observe::LoggerConfig;
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
        assert!(config.with_targets);
        assert!(config.use_color);
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
