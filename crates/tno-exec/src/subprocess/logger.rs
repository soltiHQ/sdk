/// Configuration for subprocess output logging.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    /// Max line length before truncation.
    pub max_line_length: usize,
    /// Log stdout at INFO level (false = DEBUG).
    pub stdout_info: bool,
    /// Log stderr at WARN level (false = DEBUG).
    pub stderr_warn: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_line_length: 4096,
            stdout_info: true,
            stderr_warn: true,
        }
    }
}
