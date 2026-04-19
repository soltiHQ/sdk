//! # Logger: subprocess output stream processing.
//!
//! Captures stdout/stderr from a spawned subprocess, truncates long lines, and emits them via `tracing` at configurable log levels.
//!
//! ## How it fits
//! ```text
//! run_subprocess()
//!     ├──► child.stdout.take() ──► tokio::spawn(log_stream(Stdout))
//!     └──► child.stderr.take() ──► tokio::spawn(log_stream(Stderr))
//!
//! log_stream(reader, run_id, stream_kind, config)
//!     └──► for each line:
//!           ├──► truncate_line(line, max_chars)
//!           │     ├──► short? → Cow::Borrowed (zero alloc)
//!           │     └──► long?  → Cow::Owned("prefix... (truncated N bytes)")
//!           │
//!           └──► emit via tracing:
//!                 ├──► stdout + stdout_info  → info!
//!                 ├──► stderr + stderr_warn  → warn!
//!                 └──► otherwise             → debug!
//! ```
//!
//! ## Configuration
//!
//! | Field             | Default | What it does                       |
//! |-------------------|---------|------------------------------------|
//! | `max_line_length` | 4096    | truncate lines beyond this (chars) |
//! | `stdout_info`     | true    | log stdout at INFO (else DEBUG)    |
//! | `stderr_warn`     | true    | log stderr at WARN (else DEBUG)    |

use std::borrow::Cow;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tracing::{debug, info, warn};

/// Configuration for subprocess output logging.
///
/// ## Also
///
/// - [`SubprocessBackendConfig`](super::SubprocessBackendConfig) carries `LogConfig` as a field.
/// - `log_stream` async function that reads + truncates + emits lines.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    /// Max line length (in Unicode chars) before truncation of the emitted line.
    pub max_line_length: usize,
    /// Hard byte cap per line; bytes past it are drained until next `\n`.
    pub max_line_bytes: usize,
    /// Log stdout at INFO level (`false` = DEBUG).
    pub stdout_info: bool,
    /// Log stderr at WARN level (`false` = DEBUG).
    pub stderr_warn: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: 64 * 1024,
            max_line_length: 4096,
            stdout_info: true,
            stderr_warn: true,
        }
    }
}

/// Subprocess output stream kind.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }

    fn use_elevated_level(self, config: &LogConfig) -> bool {
        match self {
            Self::Stdout => config.stdout_info,
            Self::Stderr => config.stderr_warn,
        }
    }
}

/// Log subprocess output stream line-by-line with truncation.
///
/// Spawned as a tokio task per stream (stdout / stderr).
/// Reads lines until EOF or error, emitting each via `tracing`.
pub(crate) async fn log_stream<R>(reader: R, run_id: &str, stream: StreamKind, config: &LogConfig)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let stream_name = stream.as_str();
    let mut line_count = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(256);

    loop {
        buf.clear();
        let read_result = (&mut reader)
            .take(config.max_line_bytes as u64)
            .read_until(b'\n', &mut buf)
            .await;

        let bytes_read = match read_result {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                warn!(
                    task = %run_id,
                    stream = %stream_name,
                    error = %e,
                    line_num = line_count,
                    "error while reading subprocess stream"
                );
                break;
            }
        };

        let hit_cap = bytes_read == config.max_line_bytes && !buf.ends_with(b"\n");
        if buf.ends_with(b"\n") {
            buf.pop();
            if buf.ends_with(b"\r") {
                buf.pop();
            }
        }
        let raw_line = String::from_utf8_lossy(&buf).into_owned();
        let raw_line = if hit_cap {
            format!(
                "{raw_line} ...[line exceeded {} bytes, truncated]",
                config.max_line_bytes
            )
        } else {
            raw_line
        };

        if hit_cap {
            let mut scratch = [0u8; 8 * 1024];
            loop {
                let drained = match reader.read(&mut scratch).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                if let Some(nl) = scratch[..drained].iter().position(|&b| b == b'\n') {
                    let _ = nl;
                    break;
                }
            }
        }

        let line = truncate_line(&raw_line, config.max_line_length);
        line_count += 1;

        if stream.use_elevated_level(config) {
            match stream {
                StreamKind::Stdout => info!(
                    task = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    "{}",
                    line
                ),
                StreamKind::Stderr => warn!(
                    task = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    "{}",
                    line
                ),
            }
        } else {
            debug!(
                task = %run_id,
                stream = %stream_name,
                line_num = line_count,
                "{}",
                line
            );
        }
    }

    debug!(
        task = %run_id,
        stream = %stream_name,
        total_lines = line_count,
        "stream closed"
    );
}

/// Truncate line by Unicode scalar count, safe for UTF-8.
///
/// Returns `Cow::Borrowed` when no truncation is needed (zero-alloc for the common case).
/// Reports truncated bytes (O(1)) instead of chars to avoid scanning the entire tail.
pub(crate) fn truncate_line(line: &str, max_chars: usize) -> Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        None => Cow::Borrowed(line),
        Some((i, _)) => {
            let skipped_bytes = line.len() - i;
            Cow::Owned(format!(
                "{}... (truncated {skipped_bytes} bytes)",
                &line[..i]
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_line_short_line_borrowed() {
        let result = truncate_line("hello", 10);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "hello");
    }

    #[test]
    fn truncate_line_exact_length_borrowed() {
        let result = truncate_line("hello", 5);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "hello");
    }

    #[test]
    fn truncate_line_truncates_long_line() {
        let result = truncate_line("hello world", 5);
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(&*result, "hello... (truncated 6 bytes)");
    }

    #[test]
    fn truncate_line_empty_string_borrowed() {
        let result = truncate_line("", 10);
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "");
    }

    #[test]
    fn truncate_line_unicode_cyrillic() {
        let result = truncate_line("привет", 2);
        assert_eq!(&*result, "пр... (truncated 8 bytes)");
    }

    #[test]
    fn truncate_line_unicode_hebrew() {
        let result = truncate_line("שלום", 2);
        assert_eq!(&*result, "של... (truncated 4 bytes)");
    }

    #[test]
    fn truncate_line_single_char_limit() {
        let result = truncate_line("abc", 1);
        assert_eq!(&*result, "a... (truncated 2 bytes)");
    }
}
