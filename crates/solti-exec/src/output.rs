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
//! byte limit ──► drain remainder of an oversized line
//!      ┌──┴────────────────┐
//!      ▼                   ▼
//! optional tracing     OutputSink
//! lossy UTF-8 +        exact retained bytes
//! escaped controls     + truncation status
//! ```
//!
//! UTF-8 conversion and control-character escaping are confined to the
//! opt-in tracing copy. They never modify bytes sent to [`OutputSink`].

use std::{borrow::Cow, fmt::Write as _, io};

use solti_runner::OutputSink;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, BufReader};
use tracing::{debug, info, trace, warn};

/// Configuration for workload output logging.
///
/// ## Defaults
///
/// | Field                    | Default | Meaning                                    |
/// |--------------------------|---------|--------------------------------------------|
/// | `max_line_length`        | `4096`  | Maximum raw bytes published per line       |
/// | `max_line_bytes`         | `65536` | Hard retained-byte ceiling per line        |
/// | `emit_output_to_tracing` | `false` | Copy workload lines into `tracing`         |
/// | `stdout_info`            | `true`  | Use `INFO` instead of `DEBUG` for stdout   |
/// | `stderr_warn`            | `true`  | Use `WARN` instead of `DEBUG` for stderr   |
///
/// Both size limits must be greater than zero.
/// They are validated when the runner is created.
#[derive(Debug, Clone, Copy)]
pub struct LogConfig {
    /// Maximum raw bytes published per line.
    ///
    /// This byte-based interpretation keeps arbitrary subprocess output exact.
    pub max_line_length: usize,
    /// Hard maximum retained input bytes per line.
    ///
    /// The effective published limit is the lesser of this field and
    /// [`Self::max_line_length`]. Remaining bytes are drained through the next
    /// newline and the published chunk is marked as truncated.
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
    mut reader: R,
    run_id: &str,
    stream: StreamKind,
    config: &LogConfig,
    output_sink: Option<&OutputSink>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let stream_name = stream.as_str();

    if output_sink.is_none() && !config.emit_output_to_tracing {
        let mut sink = tokio::io::sink();
        match tokio::io::copy(&mut reader, &mut sink).await {
            Ok(total_bytes) => trace!(
                event = "workload.stream_closed",
                run_id = %run_id,
                stream = %stream_name,
                total_bytes,
                "stream closed"
            ),
            Err(error) => warn!(
                event = "workload.stream_read_failed",
                run_id = %run_id,
                stream = %stream_name,
                error = %error,
                "error while draining workload stream"
            ),
        }
        return;
    }

    let mut reader = BufReader::new(reader);
    let mut line_count = 0u64;
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let retained_limit = config.max_line_length.min(config.max_line_bytes);

    loop {
        let truncated = match read_bounded_line(&mut reader, &mut buf, retained_limit).await {
            Ok(None) => break,
            Ok(Some(truncated)) => truncated,
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
        line_count += 1;

        let trace_enabled = config.emit_output_to_tracing
            && match (stream, stream.use_elevated_level(config)) {
                (StreamKind::Stdout, true) => {
                    tracing::enabled!(target: "solti_exec::workload", tracing::Level::INFO)
                }
                (StreamKind::Stderr, true) => {
                    tracing::enabled!(target: "solti_exec::workload", tracing::Level::WARN)
                }
                (_, false) => {
                    tracing::enabled!(target: "solti_exec::workload", tracing::Level::DEBUG)
                }
            };
        if trace_enabled {
            let decoded = String::from_utf8_lossy(&buf);
            let log_line = sanitize_line(&decoded);
            if stream.use_elevated_level(config) {
                match stream {
                    StreamKind::Stdout => info!(target: "solti_exec::workload",
                        event = "workload.output",
                        run_id = %run_id,
                        stream = %stream_name,
                        line_num = line_count,
                        retained_bytes = buf.len(),
                        truncated,
                        line = %log_line,
                        "workload output"
                    ),
                    StreamKind::Stderr => warn!(target: "solti_exec::workload",
                        event = "workload.output",
                        run_id = %run_id,
                        stream = %stream_name,
                        line_num = line_count,
                        retained_bytes = buf.len(),
                        truncated,
                        line = %log_line,
                        "workload output"
                    ),
                }
            } else {
                debug!(target: "solti_exec::workload",
                    event = "workload.output",
                    run_id = %run_id,
                    stream = %stream_name,
                    line_num = line_count,
                    retained_bytes = buf.len(),
                    truncated,
                    line = %log_line,
                    "workload output"
                );
            }
        }

        if let Some(sink) = output_sink {
            match (stream, truncated) {
                (StreamKind::Stdout, false) => sink.stdout_line_bytes(&buf),
                (StreamKind::Stdout, true) => sink.stdout_line_bytes_truncated(&buf),
                (StreamKind::Stderr, false) => sink.stderr_line_bytes(&buf),
                (StreamKind::Stderr, true) => sink.stderr_line_bytes_truncated(&buf),
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

/// Reads one delimiter-free byte prefix and drains any omitted suffix.
async fn read_bounded_line<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    retained_limit: usize,
) -> io::Result<Option<bool>>
where
    R: AsyncBufRead + Unpin,
{
    buf.clear();

    // Two probe bytes distinguish an exact-length line ending in either LF or
    // CRLF from a line whose content actually exceeds the retained limit.
    let probe_limit = u64::try_from(retained_limit)
        .unwrap_or(u64::MAX)
        .saturating_add(2);
    let bytes_read = (&mut *reader)
        .take(probe_limit)
        .read_until(b'\n', buf)
        .await?;
    if bytes_read == 0 {
        return Ok(None);
    }

    let terminated = buf.last() == Some(&b'\n');
    if terminated {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }

    let truncated = buf.len() > retained_limit;
    if truncated {
        buf.truncate(retained_limit);
        if !terminated {
            drain_through_newline(reader).await?;
        }
    }

    Ok(Some(truncated))
}

async fn drain_through_newline<R>(reader: &mut R) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(());
        }
    }
}

/// Escapes control characters for tracing output.
///
/// Every control character except tab is escaped. ASCII controls become
/// `\xNN`; non-ASCII controls, line and paragraph separators, and
/// bidirectional formatting controls become visible `\u{NNNN}` sequences.
///
/// Clean lines are returned without allocation.
/// The output sink never passes through this function.
pub(crate) fn sanitize_line(line: &str) -> Cow<'_, str> {
    fn needs_escape(c: char) -> bool {
        (c.is_control() && c != '\t')
            || matches!(
                c,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    }

    let Some(first) = line.find(needs_escape) else {
        return Cow::Borrowed(line);
    };

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(line.len() + 8);
    out.push_str(&line[..first]);
    for c in line[first..].chars() {
        if needs_escape(c) {
            if c.is_ascii() {
                let b = c as u8;
                out.push_str("\\x");
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 0x0f)] as char);
            } else {
                write!(out, "\\u{{{:04x}}}", c as u32).expect("writing to String cannot fail");
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use bytes::Bytes;
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

    #[test]
    fn sanitize_line_escapes_unicode_line_boundaries() {
        assert_eq!(
            &*sanitize_line("a\u{0085}b\u{009b}c\u{2028}d\u{2029}e"),
            "a\\u{0085}b\\u{009b}c\\u{2028}d\\u{2029}e"
        );
    }

    #[test]
    fn sanitize_line_escapes_bidirectional_formatting_controls() {
        let controls = "\u{061c}\u{200e}\u{200f}\u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}";
        assert_eq!(
            &*sanitize_line(controls),
            "\\u{061c}\\u{200e}\\u{200f}\\u{202a}\\u{202b}\\u{202c}\\u{202d}\\u{202e}\\u{2066}\\u{2067}\\u{2068}\\u{2069}"
        );
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
                assert!(!c.truncated);
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn log_stream_preserves_invalid_utf8_exactly() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);
        let input = [b'h', b'i', 0xff, 0xfe, b'\n'];

        log_stream(
            input.as_slice(),
            "task-binary",
            StreamKind::Stdout,
            &LogConfig::default(),
            Some(&sink),
        )
        .await;

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(chunk) => {
                assert_eq!(&chunk.line[..], &input[..input.len() - 1]);
                assert!(!chunk.truncated);
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
    async fn log_stream_publishes_exact_prefix_with_explicit_truncation() {
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
                assert_eq!(&c.line[..], b"hello");
                assert!(c.truncated);
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
            b"AAAAAAAAA\nKEEPME\n".as_slice(),
            "task-cap",
            StreamKind::Stdout,
            &cfg,
            Some(&sink),
        )
        .await;

        let mut lines = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let OutputEvent::Chunk(c) = ev {
                lines.push((c.line, c.truncated));
            }
        }
        assert_eq!(
            lines,
            vec![
                (Bytes::from_static(b"AAAAAAAA"), true),
                (Bytes::from_static(b"KEEPME"), false),
            ],
            "the line after an over-cap line must survive intact"
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
                assert_eq!(&c.line[..], b"AAAAAAAA");
                assert!(!c.truncated);
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exact_cap_line_with_crlf_is_not_truncated() {
        let cfg = LogConfig {
            max_line_bytes: 8,
            max_line_length: 8,
            ..LogConfig::default()
        };
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = output_sink(tx, 1);

        log_stream(
            b"AAAAAAAA\r\n".as_slice(),
            "task-exact-crlf",
            StreamKind::Stdout,
            &cfg,
            Some(&sink),
        )
        .await;

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(chunk) => {
                assert_eq!(&chunk.line[..], b"AAAAAAAA");
                assert!(!chunk.truncated);
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

    #[tokio::test]
    async fn workload_output_tracing_escapes_unicode_spoofing_controls() {
        let capture = Arc::new(TraceCapture::default());
        let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
        let _guard = tracing::dispatcher::set_default(&dispatch);
        let config = LogConfig {
            emit_output_to_tracing: true,
            ..LogConfig::default()
        };
        let input = "first\u{0085}second\u{2028}third\u{2029}\u{202e}spoof\n";
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(1);
        let sink = output_sink(tx, 1);

        log_stream(
            input.as_bytes(),
            "run-sanitized",
            StreamKind::Stdout,
            &config,
            Some(&sink),
        )
        .await;

        let OutputEvent::Chunk(chunk) = rx.recv().await.unwrap() else {
            panic!("expected chunk");
        };
        assert_eq!(&chunk.line[..], &input.as_bytes()[..input.len() - 1]);

        let events = capture.events.lock().unwrap();
        let (_, fields) = events
            .iter()
            .find(|(target, _)| target == "solti_exec::workload")
            .expect("opt-in workload event");
        let fields = fields.join(" ");
        assert!(fields.contains("line=first\\u{0085}second\\u{2028}third\\u{2029}\\u{202e}spoof"));
        assert!(
            ['\u{0085}', '\u{2028}', '\u{2029}', '\u{202e}']
                .into_iter()
                .all(|control| !fields.contains(control))
        );
    }
}
