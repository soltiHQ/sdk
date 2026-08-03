//! # Log formats
//!
//! [`LoggerFormat`] selects the backend installed by [`crate::init_logger`].
//!
//! ```text
//! text ─────┐
//! JSON ─────┼──► LoggerFormat ──► logger backend
//! journald ─┘
//! ```

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize, Serializer};

use crate::logger::LoggerError;

/// Output backend for the logger.
///
/// Parsing trims surrounding whitespace and ignores ASCII case.
/// `journal` is an alias for `journald`.
///
/// ## Formats
///
/// | Variant    | Backend                         | Feature    |
/// |------------|---------------------------------|------------|
/// | `Text`     | `tracing_subscriber::fmt`       | Always     |
/// | `Json`     | `tracing_subscriber::fmt::json` | Always     |
/// | `Journald` | `tracing_journald`              | `journald` |
///
/// Parsing journald without its feature returns [`LoggerError::JournaldNotEnabled`].
/// Parsing it with the feature on a non-Linux target returns `LoggerError::JournaldNotSupported`.
///
/// Serialization uses canonical lowercase names.
///
/// ## Example
///
/// ```
/// use solti_observe::LoggerFormat;
///
/// let format: LoggerFormat = " JSON ".parse().unwrap();
/// assert_eq!(format, LoggerFormat::Json);
/// assert_eq!(format.to_string(), "json");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoggerFormat {
    /// Text logs with optional ANSI colors.
    Text,
    /// Structured JSON logs.
    Json,
    /// systemd-journald output.
    ///
    /// Available with feature `journald`.
    /// Initialization is supported on Linux.
    #[cfg(feature = "journald")]
    #[cfg_attr(docsrs, doc(cfg(feature = "journald")))]
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
                #[cfg(all(feature = "journald", target_os = "linux"))]
                {
                    Ok(Self::Journald)
                }
                #[cfg(all(feature = "journald", not(target_os = "linux")))]
                {
                    Err(LoggerError::JournaldNotSupported)
                }
                #[cfg(not(feature = "journald"))]
                {
                    Err(LoggerError::JournaldNotEnabled)
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
            #[cfg(feature = "journald")]
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
    fn basic_formats_parse_case_insensitively_and_display_canonically() {
        for (lowercase, mixed_case, expected) in [
            ("text", "TEXT", LoggerFormat::Text),
            ("json", "JsOn", LoggerFormat::Json),
        ] {
            assert_eq!(LoggerFormat::from_str(lowercase).unwrap(), expected);
            assert_eq!(LoggerFormat::from_str(mixed_case).unwrap(), expected);
            assert_eq!(expected.to_string(), lowercase);
        }
    }

    #[test]
    fn journald_parse_and_serde_are_platform_specific() {
        #[cfg(all(feature = "journald", target_os = "linux"))]
        {
            assert_eq!(
                LoggerFormat::from_str("journald").unwrap(),
                LoggerFormat::Journald
            );
            assert_eq!(
                serde_json::from_str::<LoggerFormat>(r#""journald""#).unwrap(),
                LoggerFormat::Journald
            );
        }

        #[cfg(all(feature = "journald", not(target_os = "linux")))]
        {
            let err = LoggerFormat::from_str("journald").unwrap_err();
            assert!(matches!(err, LoggerError::JournaldNotSupported));
            assert!(serde_json::from_str::<LoggerFormat>(r#""journald""#).is_err());
        }

        #[cfg(not(feature = "journald"))]
        {
            let err = LoggerFormat::from_str("journald").unwrap_err();
            assert!(matches!(err, LoggerError::JournaldNotEnabled));
            assert!(serde_json::from_str::<LoggerFormat>(r#""journald""#).is_err());
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
    fn serde_uses_canonical_output_and_case_insensitive_input() {
        for (format, canonical, uppercase) in [
            (LoggerFormat::Text, r#""text""#, r#""TEXT""#),
            (LoggerFormat::Json, r#""json""#, r#""JSON""#),
        ] {
            assert_eq!(serde_json::to_string(&format).unwrap(), canonical);
            assert_eq!(
                serde_json::from_str::<LoggerFormat>(uppercase).unwrap(),
                format
            );
        }
    }
}
