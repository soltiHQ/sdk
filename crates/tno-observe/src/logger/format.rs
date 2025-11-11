use std::str::FromStr;

use crate::logger::error::LoggerError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggerFormat {
    Text,
    Json,
    Journald,
}

impl FromStr for LoggerFormat {
    type Err = LoggerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let norm = s.trim().to_ascii_lowercase();
        match norm.as_str() {
            "text" => Ok(LoggerFormat::Text),
            "json" => Ok(LoggerFormat::Json),
            "journald" | "journal" => {
                #[cfg(all(target_os = "linux", feature = "journald"))]
                {
                    Ok(LoggerFormat::Journald)
                }

                #[cfg(not(all(target_os = "linux", feature = "journald")))]
                {
                    Err(LoggerError::JournaldNotSupported)
                }
            }
            _ => Err(LoggerError::InvalidFormat(s.to_string())),
        }
    }
}
