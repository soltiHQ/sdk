use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize, Serializer};

use crate::logger::LoggerError;

/// Output format for the logger.
/// - `Text`     — human-friendly, colored (when enabled) text logs.
/// - `Json`     — structured JSON logs for machines / log collectors.
/// - `Journald` — logs are sent to systemd-journald (Linux only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoggerFormat {
    /// Human-readable text logs (default).
    Text,
    /// Structured JSON logs.
    Json,
    /// systemd-journald output (Linux only).
    Journald,
}

impl Default for LoggerFormat {
    fn default() -> Self {
        Self::Text
    }
}

impl FromStr for LoggerFormat {
    type Err = LoggerError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_lowercase();
        match norm.as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "journald" | "journal" => {
                #[cfg(target_os = "linux")]
                {
                    Ok(Self::Journald)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err(LoggerError::JournaldNotSupported)
                }
            }
            _ => Err(LoggerError::InvalidFormat(s.to_string())),
        }
    }
}

impl fmt::Display for LoggerFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LoggerFormat::Text => "text",
            LoggerFormat::Json => "json",
            LoggerFormat::Journald => "journald",
        };
        f.write_str(s)
    }
}

impl Serialize for LoggerFormat {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for LoggerFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn default_is_text() {
        assert_eq!(LoggerFormat::default(), LoggerFormat::Text);
    }

    #[test]
    fn parses_basic_formats_case_insensitive() {
        assert_eq!(LoggerFormat::from_str("text").unwrap(), LoggerFormat::Text);
        assert_eq!(LoggerFormat::from_str("TEXT").unwrap(), LoggerFormat::Text);
        assert_eq!(LoggerFormat::from_str("json").unwrap(), LoggerFormat::Json);
        assert_eq!(LoggerFormat::from_str("JsOn").unwrap(), LoggerFormat::Json);
    }

    #[test]
    fn journald_behavior_is_platform_specific() {
        #[cfg(target_os = "linux")]
        {
            assert!(LoggerFormat::from_str("journald").is_ok());
        }

        #[cfg(not(target_os = "linux"))]
        {
            let err = LoggerFormat::from_str("journald").unwrap_err();
            assert!(matches!(err, LoggerError::JournaldNotSupported));
        }
    }

    #[test]
    fn rejects_unknown_format() {
        let bad = ["", "  ", "xml", "logfmt", "text-json", "unknown"];

        for input in bad {
            let parsed = LoggerFormat::from_str(input);
            assert!(
                parsed.is_err(),
                "expected error for invalid LoggerFormat {input:?}, but got Ok"
            );
        }
    }

    #[test]
    fn display_returns_canonical_names() {
        assert_eq!(LoggerFormat::Text.to_string(), "text");
        assert_eq!(LoggerFormat::Json.to_string(), "json");
        assert_eq!(LoggerFormat::Journald.to_string(), "journald");
    }

    #[test]
    fn serde_roundtrip() {
        for fmt in [LoggerFormat::Text, LoggerFormat::Json] {
            let json = serde_json::to_string(&fmt).unwrap();
            let parsed: LoggerFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(fmt, parsed, "serde roundtrip failed for {fmt:?}");
        }
    }

    #[test]
    fn serde_platform_checks() {
        let json = r#""journald""#;

        #[cfg(target_os = "linux")]
        {
            let parsed: LoggerFormat = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, LoggerFormat::Journald);
        }

        #[cfg(not(target_os = "linux"))]
        {
            let err = serde_json::from_str::<LoggerFormat>(json);
            assert!(
                err.is_err(),
                "Journald deserialization should fail on non-Linux"
            );
        }
    }

    #[test]
    fn serde_accepts_case_insensitive_input() {
        for input in ["text", "TEXT", "Text"] {
            let json = format!(r#""{input}""#);
            let parsed: LoggerFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, LoggerFormat::Text);
        }
    }
}
