//! # Workload output
//!
//! Stdout and stderr are read as separate line streams.
//! Each line is sent to an optional [`OutputSink`]. A tracing copy is opt-in.
//!
//! ## Flow
//!
//! ```text
//! stdout/stderr bytes
//!         ▼
//! byte limit ──► drain remaining bytes in the line
//!         ▼
//! lossy UTF-8 decoding
//!         ▼
//! character limit
//!      ┌──┴────────────────┐
//!      ▼                   ▼
//! optional tracing     OutputSink
//! control bytes        decoded line
//! are escaped
//! ```
//!
//! Invalid UTF-8 uses replacement characters.
//! Control characters except tab are escaped only in the tracing copy.

use std::borrow::Cow;

use bytes::Bytes;
use solti_runner::OutputSink;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tracing::{debug, info, trace, warn};

/// Configuration for workload output logging.
///
/// ## Defaults
///
/// | Field                    | Default | Meaning                                    |
/// |--------------------------|---------|--------------------------------------------|
/// | `max_line_length`        | `4096`  | Unicode scalar values retained             |
/// | `max_line_bytes`         | `65536` | Input bytes retained before draining       |
/// | `emit_output_to_tracing` | `false` | Copy workload lines into `tracing`         |
/// | `stdout_info`            | `true`  | Use `INFO` instead of `DEBUG` for stdout   |
/// | `stderr_warn`            | `true`  | Use `WARN` instead of `DEBUG` for stderr   |
///
/// Both size limits must be greater than zero.
/// They are validated when the runner is created.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    /// Maximum emitted line length in Unicode scalar values.
    pub max_line_length: usize,
    /// Maximum retained input bytes per line.
    ///
    /// Remaining bytes are drained through the next newline.
    pub max_line_bytes: usize,
    /// Copies workload stdout and stderr into `tracing`.
    ///
    /// This is disabled by default. When enabled, records use the separate
    /// `solti_exec::workload` target. The output sink is independent of this
    /// setting and continues to receive every retained line.
    pub emit_output_to_tracing: bool,
    /// Logs stdout at `INFO` when the tracing copy is enabled.
    ///
    /// `false` uses `DEBUG`.
    pub stdout_info: bool,
    /// Logs stderr at `WARN` when the tracing copy is enabled.
    ///
    /// `false` uses `DEBUG`.
    pub stderr_warn: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: 64 * 1024,
            max_line_length: 4096,
            emit_output_to_tracing: false,
            stdout_info: true,
            stderr_warn: true,
        }
    }
}

/// Captured workload stream.
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

/// Reads, limits, and publishes one workload stream.
pub(crate) async fn log_stream<R>(
    reader: R,
    run_id: &str,
    stream: StreamKind,
    config: &LogConfig,
    output_sink: Option<&OutputSink>,
) where
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
                    event = "workload.stream_read_failed",
                    run_id = %run_id,
                    stream = %stream_name,
                    error = %e,
                    line_num = line_count,
                    "error while reading workload stream"
                );
                break;
            }
        };

        let mut hit_cap = bytes_read == config.max_line_bytes && !buf.ends_with(b"\n");
        if buf.ends_with(b"\n") {
            buf.pop();
            if buf.ends_with(b"\r") {
                buf.pop();
            }
        }

        if hit_cap {
            let mut drained_any = false;
            let mut junk: Vec<u8> = Vec::with_capacity(256);
            loop {
                junk.clear();
                match (&mut reader)
                    .take(config.max_line_bytes as u64)
                    .read_until(b'\n', &mut junk)
                    .await
                {
                    Ok(0) => break,
                    Ok(_) => {
                        drained_any = true;
                        if junk.last() == Some(&b'\n') {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            if !drained_any {
                hit_cap = false;
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

        let line = truncate_line(&raw_line, config.max_line_length);
        let log_line = sanitize_line(&line);
        line_count += 1;

        if config.emit_output_to_tracing && stream.use_elevated_level(config) {
            match stream {
                StreamKind::Stdout => info!(target: "solti_exec::workload",
                    event = "workload.output",
                    run_id = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    line = %log_line,
                    "workload output"
                ),
                StreamKind::Stderr => warn!(target: "solti_exec::workload",
                    event = "workload.output",
                    run_id = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    line = %log_line,
                    "workload output"
                ),
            }
        } else if config.emit_output_to_tracing {
            debug!(target: "solti_exec::workload",
                event = "workload.output",
                run_id = %run_id,
                stream = %stream_name,
                line_num = line_count,
                line = %log_line,
                "workload output"
            );
        }

        if let Some(sink) = output_sink {
            let bytes_line: Bytes = match line {
                Cow::Borrowed(s) => Bytes::copy_from_slice(s.as_bytes()),
                Cow::Owned(s) => Bytes::from(s),
            };
            match stream {
                StreamKind::Stdout => sink.stdout_line(bytes_line),
                StreamKind::Stderr => sink.stderr_line(bytes_line),
            }
        }
    }

    trace!(
        event = "workload.stream_closed",
        run_id = %run_id,
        stream = %stream_name,
        total_lines = line_count,
        "stream closed"
    );
}

/// Escapes control characters for tracing output.
///
/// Every ASCII control character except tab becomes a `\xNN` sequence.
///
/// Clean lines are returned without allocation.
/// The output sink receives the unsanitized limited line.
pub(crate) fn sanitize_line(line: &str) -> Cow<'_, str> {
    fn needs_escape(c: char) -> bool {
        c.is_ascii_control() && c != '\t'
    }

    let Some(first) = line.find(needs_escape) else {
        return Cow::Borrowed(line);
    };

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(line.len() + 8);
    out.push_str(&line[..first]);
    for c in line[first..].chars() {
        if needs_escape(c) {
            let b = c as u8;
            out.push_str("\\x");
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 0x0f)] as char);
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Truncates a line by Unicode scalar count.
///
/// Short lines are returned without allocation.
/// The suffix reports the number of removed bytes.
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

    use solti_model::OutputEvent;
    use solti_runner::OutputSink;
    use std::{
        fmt,
        sync::{Arc, Mutex},
    };
    use tokio::sync::broadcast;
    use tracing::{
        Event, Metadata, Subscriber,
        field::{Field, Visit},
        span::{Attributes, Id, Record},
    };

    #[derive(Default)]
    struct TraceCapture {
        events: Mutex<Vec<(String, Vec<String>)>>,
    }

    struct CaptureSubscriber(Arc<TraceCapture>);

    impl Subscriber for CaptureSubscriber {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = Vec::new();
            event.record(&mut CaptureVisitor(&mut fields));
            self.0
                .events
                .lock()
                .unwrap()
                .push((event.metadata().target().to_owned(), fields));
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    struct CaptureVisitor<'a>(&'a mut Vec<String>);

    impl Visit for CaptureVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.0.push(format!("{}={value:?}", field.name()));
        }
    }

    fn output_sink(sender: broadcast::Sender<OutputEvent>, attempt: u32) -> OutputSink {
        OutputSink::new(1, attempt, move |event| {
            let _ = sender.send(event);
        })
    }

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

    #[test]
    fn sanitize_line_clean_line_borrowed() {
        let result = sanitize_line("hello world");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "hello world");
    }

    #[test]
    fn sanitize_line_escapes_ansi_sequence() {
        let result = sanitize_line("\x1b[31mred\x1b[0m");
        assert!(matches!(result, Cow::Owned(_)));
        assert_eq!(&*result, "\\x1b[31mred\\x1b[0m");
    }

    #[test]
    fn sanitize_line_preserves_tab() {
        let result = sanitize_line("col1\tcol2");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "col1\tcol2");
    }

    #[test]
    fn sanitize_line_escapes_carriage_return() {
        assert_eq!(&*sanitize_line("fake\rline"), "fake\\x0dline");
    }

    #[test]
    fn sanitize_line_escapes_del_and_nul() {
        assert_eq!(&*sanitize_line("a\x7fb"), "a\\x7fb");
        assert_eq!(&*sanitize_line("a\0b"), "a\\x00b");
    }

    #[test]
    fn sanitize_line_unicode_passes_through_borrowed() {
        let result = sanitize_line("привет мир");
        assert!(matches!(result, Cow::Borrowed(_)));
        assert_eq!(&*result, "привет мир");
    }

    #[test]
    fn sanitize_line_control_between_unicode_chars() {
        assert_eq!(&*sanitize_line("привет\x1bмир"), "привет\\x1bмир");
    }

    #[tokio::test]
    async fn log_stream_sink_receives_raw_control_bytes() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        log_stream(
            "\x1b[31mred\x1b[0m\n".as_bytes(),
            "task-raw",
            StreamKind::Stdout,
            &LogConfig::default(),
            Some(&sink),
        )
        .await;

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(c) => {
                assert_eq!(
                    &c.line[..],
                    b"\x1b[31mred\x1b[0m",
                    "broadcast path must carry raw bytes, not the sanitized tracing copy"
                );
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn log_stream_pushes_each_stdout_line_to_sink() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        let reader = "alpha\nbeta\ngamma\n".as_bytes();
        log_stream(
            reader,
            "task-1",
            StreamKind::Stdout,
            &LogConfig::default(),
            Some(&sink),
        )
        .await;

        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let OutputEvent::Chunk(c) = ev {
                assert_eq!(c.stream, solti_model::StreamKind::Stdout);
                lines.push(std::str::from_utf8(&c.line).unwrap().to_string());
            }
        }
        assert_eq!(lines, vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn log_stream_pushes_stderr_line_with_stderr_kind() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        log_stream(
            "boom\n".as_bytes(),
            "task-2",
            StreamKind::Stderr,
            &LogConfig::default(),
            Some(&sink),
        )
        .await;

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(c) => {
                assert_eq!(c.stream, solti_model::StreamKind::Stderr);
                assert_eq!(&c.line[..], b"boom");
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn log_stream_pushes_truncated_line_not_raw() {
        let cfg = LogConfig {
            max_line_length: 5,
            ..LogConfig::default()
        };
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        log_stream(
            "hello world\n".as_bytes(),
            "task-3",
            StreamKind::Stdout,
            &cfg,
            Some(&sink),
        )
        .await;

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(c) => {
                let line_text = std::str::from_utf8(&c.line).expect("line must be UTF-8");
                assert!(
                    line_text.starts_with("hello"),
                    "expected truncated, got {line_text:?}"
                );
                assert!(line_text.contains("truncated"));
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn log_stream_over_cap_line_does_not_eat_the_following_line() {
        let cfg = LogConfig {
            max_line_bytes: 8,
            ..LogConfig::default()
        };
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        log_stream(
            b"AAAAAAAA\nKEEPME\n".as_slice(),
            "task-cap",
            StreamKind::Stdout,
            &cfg,
            Some(&sink),
        )
        .await;

        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let OutputEvent::Chunk(c) = ev {
                lines.push(String::from_utf8_lossy(&c.line).into_owned());
            }
        }
        assert!(
            lines.iter().any(|l| l == "KEEPME"),
            "the line after an over-cap line must survive intact, got {lines:?}"
        );
    }

    #[tokio::test]
    async fn log_stream_exact_cap_final_line_at_eof_is_not_marked_truncated() {
        let cfg = LogConfig {
            max_line_bytes: 8,
            ..LogConfig::default()
        };
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        log_stream(
            b"AAAAAAAA".as_slice(),
            "task-exact",
            StreamKind::Stdout,
            &cfg,
            Some(&sink),
        )
        .await;

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(c) => {
                let s = String::from_utf8_lossy(&c.line);
                assert_eq!(
                    s, "AAAAAAAA",
                    "a complete exact-cap line must not be marked truncated"
                );
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn log_stream_with_none_sink_is_a_noop_for_subscribers() {
        log_stream(
            "noisy\n".as_bytes(),
            "task-4",
            StreamKind::Stdout,
            &LogConfig::default(),
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn workload_output_tracing_is_disabled_by_default() {
        let capture = Arc::new(TraceCapture::default());
        let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
        let _guard = tracing::dispatcher::set_default(&dispatch);

        log_stream(
            "not-a-diagnostic\n".as_bytes(),
            "run-default",
            StreamKind::Stdout,
            &LogConfig::default(),
            None,
        )
        .await;

        assert!(
            capture
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|(target, _)| target != "solti_exec::workload")
        );
    }

    #[tokio::test]
    async fn workload_output_tracing_uses_dedicated_target_when_enabled() {
        let capture = Arc::new(TraceCapture::default());
        let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let config = LogConfig {
            emit_output_to_tracing: true,
            ..LogConfig::default()
        };

        log_stream(
            "visible-line\n".as_bytes(),
            "run-opt-in",
            StreamKind::Stdout,
            &config,
            None,
        )
        .await;

        let events = capture.events.lock().unwrap();
        let (_, fields) = events
            .iter()
            .find(|(target, _)| target == "solti_exec::workload")
            .expect("opt-in workload event");
        let fields = fields.join(" ");
        assert!(fields.contains("run_id=run-opt-in"));
        assert!(fields.contains("line=visible-line"));
    }
}
