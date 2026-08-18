//! # Runner output
//!
//! Runners publish stdout and stderr chunks.
//! They do not own channels, subscriptions, or lifecycle events.
//!
//! ## Flow
//!
//! ```text
//! BuildContext
//!      ▼
//! OutputPublisher ── task + generation + attempt ──▶ OutputSink
//!                                                    │
//!                                              stdout/stderr bytes
//!                                                    ▼
//!                                            LF / CRLF framing
//!                                                    ▼
//!                                              OutputEvent::Chunk
//! ```
//!
//! The composition layer provides [`OutputPublisher`].
//! Runners request [`OutputSink`] values by task, generation, and attempt.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use solti_model::{OutputChunk, OutputEvent, StreamKind, TaskId};

/// Producer capability used by runners to obtain an attempt-scoped output sink.
///
/// Implementations decide whether output is enabled for an attempt.
/// Returning `None` disables output without changing task execution.
///
/// Request the sink from the task attempt future before moving output work into
/// a separately spawned task. Composition runners may route output through
/// execution-local context that a new task does not inherit. [`OutputSink`] is
/// cloneable and its clones can be moved into reader or forwarding tasks.
///
/// This interface has no subscription or lifecycle operations.
pub trait OutputPublisher: Send + Sync {
    /// Returns a sink for one task attempt.
    ///
    /// Returns `None` when output is disabled.
    /// Call this from the task attempt future, then clone the returned sink for
    /// any separately spawned output work.
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink>;
}

/// Shared output producer capability injected into runners.
pub type OutputPublisherHandle = Arc<dyn OutputPublisher>;

/// Returns an output publisher that disables live output.
pub fn noop_output_publisher() -> OutputPublisherHandle {
    Arc::new(NoOpOutputPublisher)
}

#[derive(Debug)]
struct NoOpOutputPublisher;

impl OutputPublisher for NoOpOutputPublisher {
    fn sink_for(&self, _task_name: &TaskId, _generation: u64, _attempt: u32) -> Option<OutputSink> {
        None
    }
}

/// Borrowed view of one attempt-scoped output chunk.
///
/// The view is valid only for the duration of the synchronous callback passed
/// to [`OutputSink::new_borrowed`]. Copy [`Self::line`] when the bytes must be
/// retained after that callback returns.
#[derive(Debug, Clone, Copy)]
pub struct OutputChunkRef<'a> {
    generation: u64,
    attempt: u32,
    stream: StreamKind,
    seq: u64,
    ts: SystemTime,
    line: &'a [u8],
    truncated: bool,
}

impl<'a> OutputChunkRef<'a> {
    /// Returns the desired-state generation.
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Returns the attempt number.
    pub const fn attempt(self) -> u32 {
        self.attempt
    }

    /// Returns the source stream.
    pub const fn stream(self) -> StreamKind {
        self.stream
    }

    /// Returns the sequence number within this generation, attempt, and stream.
    pub const fn seq(self) -> u64 {
        self.seq
    }

    /// Returns the wall-clock publication time.
    pub const fn timestamp(self) -> SystemTime {
        self.ts
    }

    /// Returns the exact retained line bytes without an LF or CRLF delimiter.
    pub const fn line(self) -> &'a [u8] {
        self.line
    }

    /// Returns whether bytes were omitted from the end of the source line.
    pub const fn truncated(self) -> bool {
        self.truncated
    }
}

type OwnedPublish = dyn Fn(OutputEvent) + Send + Sync;
type BorrowedPublish = dyn for<'a> Fn(OutputChunkRef<'a>) + Send + Sync;

#[derive(Clone)]
enum Publish {
    Owned(Arc<OwnedPublish>),
    Borrowed(Arc<BorrowedPublish>),
}

/// Write-only output sink for one task attempt.
///
/// Each write creates one or more [`OutputEvent::Chunk`] values.
/// The chunk uses the sink generation and attempt.
/// Its timestamp comes from [`SystemTime::now`].
///
/// Stdout and stderr have independent sequence counters.
/// Both counters start at `0` and wrap on `u64` overflow.
/// Cloned sinks share those counters.
///
/// ## Framing
///
/// Every LF terminates one emitted chunk. A CR immediately before that LF is
/// part of the delimiter; every other byte remains exact. Empty input emits
/// one empty chunk. A trailing delimiter terminates its preceding chunk and
/// does not synthesize another. Consecutive delimiters therefore preserve
/// intervening empty lines.
///
/// A truncated write marks only its final emitted chunk, including when the
/// input ends in a delimiter. Sequence numbers are assigned per emitted chunk.
///
/// The callback runs synchronously in the caller.
/// It must not block runner execution.
///
/// ## Example
///
/// ```
/// use std::sync::{Arc, Mutex};
///
/// use bytes::Bytes;
/// use solti_model::OutputEvent;
/// use solti_runner::OutputSink;
///
/// let events = Arc::new(Mutex::new(Vec::new()));
/// let captured = Arc::clone(&events);
/// let sink = OutputSink::new(4, 2, move |event| {
///     captured.lock().unwrap().push(event);
/// });
///
/// sink.stdout_line(Bytes::from_static(b"ready"));
///
/// assert!(matches!(
///     &events.lock().unwrap()[0],
///     OutputEvent::Chunk(_)
/// ));
/// ```
#[derive(Clone)]
pub struct OutputSink {
    generation: u64,
    attempt: u32,
    seq_stdout: Arc<AtomicU64>,
    seq_stderr: Arc<AtomicU64>,
    publish: Publish,
}

impl OutputSink {
    /// Creates a sink with a synchronous event callback.
    ///
    /// This constructor is intended for [`OutputPublisher`] implementations.
    /// The callback must not block runner execution.
    pub fn new<F>(generation: u64, attempt: u32, publish: F) -> Self
    where
        F: Fn(OutputEvent) + Send + Sync + 'static,
    {
        Self {
            generation,
            attempt,
            seq_stdout: Arc::new(AtomicU64::new(0)),
            seq_stderr: Arc::new(AtomicU64::new(0)),
            publish: Publish::Owned(Arc::new(publish)),
        }
    }

    /// Creates a sink with a synchronous borrowed-chunk callback.
    ///
    /// This constructor lets a composition layer copy a chunk directly into
    /// its bounded storage without an intermediate payload allocation. The
    /// callback cannot retain [`OutputChunkRef::line`] after it returns.
    ///
    /// The callback must not block runner execution.
    pub fn new_borrowed<F>(generation: u64, attempt: u32, publish: F) -> Self
    where
        F: for<'a> Fn(OutputChunkRef<'a>) + Send + Sync + 'static,
    {
        Self {
            generation,
            attempt,
            seq_stdout: Arc::new(AtomicU64::new(0)),
            seq_stderr: Arc::new(AtomicU64::new(0)),
            publish: Publish::Borrowed(Arc::new(publish)),
        }
    }

    /// Publishes stdout bytes as one or more delimiter-free chunks.
    ///
    /// LF and CRLF delimiters split the input without changing other bytes.
    /// Each emitted chunk receives the next stdout sequence number.
    pub fn stdout_line(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stdout, line, false);
    }

    /// Publishes stdout bytes whose final source line was truncated.
    ///
    /// LF and CRLF delimiters split the input. Only the final emitted chunk is
    /// marked as truncated; every emitted chunk receives its own sequence.
    pub fn stdout_line_truncated(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stdout, line, true);
    }

    /// Publishes stderr bytes as one or more delimiter-free chunks.
    ///
    /// LF and CRLF delimiters split the input without changing other bytes.
    /// Each emitted chunk receives the next stderr sequence number.
    pub fn stderr_line(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stderr, line, false);
    }

    /// Publishes stderr bytes whose final source line was truncated.
    ///
    /// LF and CRLF delimiters split the input. Only the final emitted chunk is
    /// marked as truncated; every emitted chunk receives its own sequence.
    pub fn stderr_line_truncated(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stderr, line, true);
    }

    /// Publishes borrowed stdout bytes as delimiter-free chunks.
    ///
    /// This has the same framing and sequence semantics as [`Self::stdout_line`].
    /// An owned-callback sink copies each emitted chunk once. A
    /// borrowed-callback sink does not allocate payload storage.
    pub fn stdout_line_bytes(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stdout, line, false);
    }

    /// Publishes borrowed stdout bytes whose final source line was truncated.
    ///
    /// This has the same framing, truncation, and sequence semantics as
    /// [`Self::stdout_line_truncated`].
    pub fn stdout_line_bytes_truncated(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stdout, line, true);
    }

    /// Publishes borrowed stderr bytes as delimiter-free chunks.
    ///
    /// This has the same framing and sequence semantics as [`Self::stderr_line`].
    /// An owned-callback sink copies each emitted chunk once. A
    /// borrowed-callback sink does not allocate payload storage.
    pub fn stderr_line_bytes(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stderr, line, false);
    }

    /// Publishes borrowed stderr bytes whose final source line was truncated.
    ///
    /// This has the same framing, truncation, and sequence semantics as
    /// [`Self::stderr_line_truncated`].
    pub fn stderr_line_bytes_truncated(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stderr, line, true);
    }

    /// Returns the attempt number.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the desired-state generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn push_bytes(&self, stream: StreamKind, line: Bytes, truncated: bool) {
        for_each_line_range(&line, |range, final_chunk| {
            self.push_owned(stream, line.slice(range), truncated && final_chunk);
        });
    }

    fn push_slice(&self, stream: StreamKind, line: &[u8], truncated: bool) {
        for_each_line_range(line, |range, final_chunk| {
            self.push_borrowed(stream, &line[range], truncated && final_chunk);
        });
    }

    fn next_seq(&self, stream: StreamKind) -> u64 {
        match stream {
            StreamKind::Stdout => self.seq_stdout.fetch_add(1, Ordering::Relaxed),
            StreamKind::Stderr => self.seq_stderr.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn push_owned(&self, stream: StreamKind, line: Bytes, truncated: bool) {
        let seq = self.next_seq(stream);
        let ts = SystemTime::now();
        match &self.publish {
            Publish::Owned(publish) => publish(OutputEvent::Chunk(OutputChunk {
                generation: self.generation,
                attempt: self.attempt,
                stream,
                seq,
                ts,
                line,
                truncated,
            })),
            Publish::Borrowed(publish) => publish(OutputChunkRef {
                generation: self.generation,
                attempt: self.attempt,
                stream,
                seq,
                ts,
                line: &line,
                truncated,
            }),
        }
    }

    fn push_borrowed(&self, stream: StreamKind, line: &[u8], truncated: bool) {
        let seq = self.next_seq(stream);
        let ts = SystemTime::now();
        match &self.publish {
            Publish::Owned(publish) => publish(OutputEvent::Chunk(OutputChunk {
                generation: self.generation,
                attempt: self.attempt,
                stream,
                seq,
                ts,
                line: Bytes::copy_from_slice(line),
                truncated,
            })),
            Publish::Borrowed(publish) => publish(OutputChunkRef {
                generation: self.generation,
                attempt: self.attempt,
                stream,
                seq,
                ts,
                line,
                truncated,
            }),
        }
    }
}

fn for_each_line_range(line: &[u8], mut publish: impl FnMut(std::ops::Range<usize>, bool)) {
    if line.is_empty() {
        publish(0..0, true);
        return;
    }

    let mut start = 0;
    while let Some(relative_end) = line[start..].iter().position(|byte| *byte == b'\n') {
        let end = start + relative_end;
        let content_end = if end > start && line[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };
        publish(start..content_end, end + 1 == line.len());
        start = end + 1;
    }
    if start < line.len() {
        publish(start..line.len(), true);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use solti_model::{OutputEvent, StreamKind, TaskId};

    use super::{OutputSink, noop_output_publisher};

    fn recording_sink(generation: u64, attempt: u32) -> (OutputSink, Arc<Mutex<Vec<OutputEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let sink = OutputSink::new(generation, attempt, move |event| {
            recorded.lock().unwrap().push(event);
        });
        (sink, events)
    }

    #[test]
    fn sink_emits_attempt_chunks_with_shared_per_stream_sequences() {
        let (sink, events) = recording_sink(2, 3);
        let clone = sink.clone();
        let first = Bytes::from_static(b"hello");
        let first_pointer = first.as_ptr();

        sink.stdout_line(first);
        clone.stdout_line(Bytes::from_static(b"again"));
        clone.stderr_line(Bytes::from_static(b"oops"));
        sink.stderr_line_truncated(Bytes::from_static(b"retry"));

        let events = events.lock().unwrap();
        let chunks = events
            .iter()
            .map(|event| match event {
                OutputEvent::Chunk(chunk) => {
                    assert_eq!(chunk.generation, 2);
                    assert_eq!(chunk.attempt, 3);
                    (
                        chunk.stream,
                        chunk.seq,
                        chunk.line.as_ref(),
                        chunk.truncated,
                    )
                }
                other => panic!("expected chunk, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            vec![
                (StreamKind::Stdout, 0, b"hello".as_slice(), false),
                (StreamKind::Stdout, 1, b"again".as_slice(), false),
                (StreamKind::Stderr, 0, b"oops".as_slice(), false),
                (StreamKind::Stderr, 1, b"retry".as_slice(), true),
            ]
        );
        let OutputEvent::Chunk(first_chunk) = &events[0] else {
            panic!("expected chunk");
        };
        assert_eq!(first_chunk.line.as_ptr(), first_pointer);
    }

    #[test]
    fn sink_frames_empty_lf_and_crlf_as_empty_lines() {
        let (sink, events) = recording_sink(1, 1);

        sink.stdout_line(Bytes::new());
        sink.stdout_line(Bytes::from_static(b"\n"));
        sink.stdout_line(Bytes::from_static(b"\r\n"));

        let events = events.lock().unwrap();
        for (seq, event) in events.iter().enumerate() {
            let OutputEvent::Chunk(chunk) = event else {
                panic!("expected chunk");
            };
            assert_eq!(chunk.stream, StreamKind::Stdout);
            assert_eq!(chunk.seq, seq as u64);
            assert!(chunk.line.is_empty());
            assert!(!chunk.truncated);
        }
    }

    #[test]
    fn sink_splits_embedded_trailing_and_consecutive_delimiters() {
        let (sink, events) = recording_sink(1, 1);
        let clone = sink.clone();

        sink.stdout_line(Bytes::from_static(b"alpha\nbeta\r\n"));
        clone.stdout_line(Bytes::from_static(b"gamma\n\ndelta"));
        clone.stderr_line(Bytes::from_static(b"error\nretry"));

        let events = events.lock().unwrap();
        let chunks = events
            .iter()
            .map(|event| match event {
                OutputEvent::Chunk(chunk) => {
                    (chunk.stream, chunk.seq, chunk.line.clone(), chunk.truncated)
                }
                other => panic!("expected chunk, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            vec![
                (StreamKind::Stdout, 0, Bytes::from_static(b"alpha"), false,),
                (StreamKind::Stdout, 1, Bytes::from_static(b"beta"), false,),
                (StreamKind::Stdout, 2, Bytes::from_static(b"gamma"), false,),
                (StreamKind::Stdout, 3, Bytes::new(), false),
                (StreamKind::Stdout, 4, Bytes::from_static(b"delta"), false,),
                (StreamKind::Stderr, 0, Bytes::from_static(b"error"), false,),
                (StreamKind::Stderr, 1, Bytes::from_static(b"retry"), false,),
            ]
        );
        assert!(chunks.iter().all(|(_, _, line, _)| !line.contains(&b'\n')));
    }

    #[test]
    fn sink_preserves_lone_cr_and_non_utf8_bytes() {
        let (sink, events) = recording_sink(1, 1);

        sink.stdout_line(Bytes::from_static(b"lone\r"));
        sink.stdout_line(Bytes::from_static(&[0xff, 0xfe, b'X']));

        let events = events.lock().unwrap();
        let lines = events
            .iter()
            .map(|event| match event {
                OutputEvent::Chunk(chunk) => chunk.line.clone(),
                other => panic!("expected chunk, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                Bytes::from_static(b"lone\r"),
                Bytes::from_static(&[0xff, 0xfe, b'X']),
            ]
        );
    }

    #[test]
    fn truncated_marks_only_the_last_chunk_of_split_input() {
        let (sink, events) = recording_sink(1, 1);

        sink.stdout_line_truncated(Bytes::from_static(b"complete\r\nprefix"));
        sink.stderr_line_truncated(Bytes::from_static(b"terminated\n"));

        let events = events.lock().unwrap();
        let chunks = events
            .iter()
            .map(|event| match event {
                OutputEvent::Chunk(chunk) => {
                    (chunk.stream, chunk.seq, chunk.line.clone(), chunk.truncated)
                }
                other => panic!("expected chunk, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            vec![
                (
                    StreamKind::Stdout,
                    0,
                    Bytes::from_static(b"complete"),
                    false,
                ),
                (StreamKind::Stdout, 1, Bytes::from_static(b"prefix"), true,),
                (
                    StreamKind::Stderr,
                    0,
                    Bytes::from_static(b"terminated"),
                    true,
                ),
            ]
        );
    }

    #[test]
    fn borrowed_sink_observes_the_callers_buffer_without_an_intermediate_copy() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&observed);
        let sink = OutputSink::new_borrowed(7, 3, move |chunk| {
            captured.lock().unwrap().push((
                chunk.generation(),
                chunk.attempt(),
                chunk.stream(),
                chunk.seq(),
                chunk.line().as_ptr() as usize,
                chunk.line().to_vec(),
                chunk.truncated(),
            ));
        });
        let line = b"borrowed\nsecond";

        sink.stdout_line_bytes(line);

        assert_eq!(
            &*observed.lock().unwrap(),
            &[
                (
                    7,
                    3,
                    StreamKind::Stdout,
                    0,
                    line.as_ptr() as usize,
                    b"borrowed".to_vec(),
                    false,
                ),
                (
                    7,
                    3,
                    StreamKind::Stdout,
                    1,
                    line[9..].as_ptr() as usize,
                    b"second".to_vec(),
                    false,
                ),
            ]
        );
    }

    #[test]
    fn noop_publisher_disables_output() {
        assert!(
            noop_output_publisher()
                .sink_for(&TaskId::new("task-1").unwrap(), 1, 1)
                .is_none()
        );
    }
}
