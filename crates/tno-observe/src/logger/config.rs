use crate::logger::format::LoggerFormat;

#[derive(Debug, Clone)]
pub struct LoggerConfig {
    pub format: LoggerFormat,
    pub level: String,
    pub with_targets: bool,
    pub use_color: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        let use_color = cfg!(test) || atty::is(atty::Stream::Stdout);
        Self {
            format: LoggerFormat::Text,
            level: "info".to_string(),
            with_targets: true,
            use_color,
        }
    }
}
