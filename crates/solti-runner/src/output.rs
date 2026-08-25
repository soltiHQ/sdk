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

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use solti_model::{OutputChunk, OutputEvent, StreamKind, TaskId};

use crate::callback::{
    CallbackPanicFuse, PanicPayload, dispose_panic_payload, report_without_unwind,
};

/// Producer capability used by runners to obtain an attempt-scoped output sink.
///
/// Implementations decide whether output is enabled for an attempt.
/// Returning `None` disables output without changing task execution.
///
/// Request the sink from the task attempt future before moving output work into a separately spawned task.
/// Composition runners may route output through execution-local context that a new task does not inherit.
/// [`OutputSink`] is cloneable and its clones can be moved into reader or forwarding tasks.
///
/// This interface has no subscription or lifecycle operations.
/// Implementations must not panic; SDK containment is a defensive boundary.
/// SDK-owned runner paths invoke it through [`request_output_sink`], which contains unwinding
/// publisher panics. Direct application calls to [`Self::sink_for`] are not mediated by that
/// boundary. Calls that already entered an installed sticky boundary concurrently may still
/// finish or panic; the boundary does not serialize healthy publishers.
pub trait OutputPublisher: Send + Sync {
    /// Returns a sink for one task attempt.
    ///
    /// Returns `None` when output is disabled.
    /// Call this from the task attempt future, then clone the returned sink for any separately spawned output work.
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink>;
}

/// Shared output producer capability injected into runners.
pub type OutputPublisherHandle = Arc<dyn OutputPublisher>;

/// Returns an output publisher that disables live output.
pub fn noop_output_publisher() -> OutputPublisherHandle {
    Arc::new(NoOpOutputPublisher)
}

/// Requests an attempt-scoped output sink without allowing a publisher panic to unwind.
///
/// SDK-owned runner paths use this boundary instead of calling [`OutputPublisher::sink_for`]
/// directly. An unwinding publisher panic is isolated, its opaque payload is discarded, and
/// the request returns `None`. The failure is reported through non-unwinding structured tracing.
/// A later request may invoke a raw publisher again. Publishers installed through
/// [`crate::BuildContext`] share a sticky panic fuse and are not invoked again after the first
/// observed panic.
/// If destroying a hostile payload itself panics, that replacement payload is intentionally
/// forgotten to prevent another unwind.
///
/// Direct application calls to [`OutputPublisher::sink_for`] are not mediated by this function.
/// The process panic hook still runs before the unwind is caught. A process built with
/// `panic = "abort"` cannot isolate a publisher panic.
pub fn request_output_sink(
    publisher: &OutputPublisherHandle,
    task_name: &TaskId,
    generation: u64,
    attempt: u32,
) -> Option<OutputSink> {
    match catch_unwind(AssertUnwindSafe(|| {
        publisher.sink_for(task_name, generation, attempt)
    })) {
        Ok(sink) => sink,
        Err(payload) => {
            dispose_panic_payload(payload);
            report_without_unwind(|| {
                tracing::error!(
                    event = "runner.output_publisher_panicked",
                    error_kind = "callback_panicked",
                    task = %task_name,
                    generation,
                    attempt,
                    "output publisher panicked; disabling output for this sink request"
                );
            });
            None
        }
    }
}

/// Installs one sticky panic boundary around an application output publisher.
pub(crate) fn panic_contained_output_publisher(
    publisher: OutputPublisherHandle,
) -> OutputPublisherHandle {
    Arc::new(PanicContainedOutputPublisher {
        publisher,
        panic_fuse: CallbackPanicFuse::default(),
    })
}

struct PanicContainedOutputPublisher {
    publisher: OutputPublisherHandle,
    panic_fuse: CallbackPanicFuse,
}

impl OutputPublisher for PanicContainedOutputPublisher {
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        if self.panic_fuse.is_disabled() {
            return None;
        }

        match catch_unwind(AssertUnwindSafe(|| {
            self.publisher.sink_for(task_name, generation, attempt)
        })) {
            Ok(sink) => sink,
            Err(payload) => {
                let report = self.panic_fuse.trip();
                dispose_panic_payload(payload);
                if report {
                    report_without_unwind(|| {
                        tracing::error!(
                            event = "runner.output_publisher_panicked",
                            error_kind = "callback_panicked",
                            task = %task_name,
                            generation,
                            attempt,
                            "output publisher panicked; disabling the installed publisher"
                        );
                    });
                }
                None
            }
        }
    }
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
/// The view is valid only for the duration of the synchronous callback passed to [`OutputSink::new_borrowed`].
/// Copy [`Self::line`] when the bytes must be retained after that callback returns.
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
/// Both counters start at `0` and never wrap.
/// Cloned sinks share those counters.
/// After a stream emits sequence `u64::MAX`, its next emission panics before
/// invoking the callback so an earlier sequence cannot be reused.
///
/// ## Framing
///
/// Every LF terminates one emitted chunk.
/// A CR immediately before that LF is part of the delimiter; every other byte remains exact.
/// Empty input emits one empty chunk.
/// A trailing delimiter terminates its preceding chunk and does not synthesize another.
/// Consecutive delimiters therefore preserve intervening empty lines.
///
/// A truncated write marks only its final emitted chunk, including when the input ends in a delimiter.
/// Sequence numbers are assigned per emitted chunk.
///
/// The callback runs synchronously in the caller.
/// It must not block runner execution.
/// It should not panic; containment is a defensive boundary.
/// An unwinding panic is caught and reported once through structured tracing without its payload.
/// Once the panic is observed, the sink disables its callback and drops calls that begin afterward.
/// Concurrent callback calls already in progress may still complete or panic.
/// Clones share that sticky state through [`Self::callback_panicked`].
/// If destroying a hostile payload itself panics, that replacement payload is intentionally
/// forgotten to prevent another unwind.
/// The process panic hook still runs before the unwind is caught; its output is controlled by the application.
/// A process built with `panic = "abort"` cannot isolate a callback panic.
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
    seq_stdout: Arc<OutputSequence>,
    seq_stderr: Arc<OutputSequence>,
    callback_panicked: Arc<AtomicBool>,
    publish: Publish,
}

struct OutputSequence {
    next: AtomicU64,
    max_issued: AtomicBool,
}

impl OutputSequence {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            max_issued: AtomicBool::new(false),
        }
    }

    fn allocate(&self) -> u64 {
        loop {
            let current = self.next.load(Ordering::Relaxed);
            let Some(next) = current.checked_add(1) else {
                if self
                    .max_issued
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return current;
                }
                panic!("output sequence exhausted; ordering cannot wrap safely");
            };

            if self
                .next
                .compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return current;
            }
        }
    }

    #[cfg(test)]
    fn seeded(next: u64) -> Self {
        Self {
            next: AtomicU64::new(next),
            max_issued: AtomicBool::new(false),
        }
    }
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
            seq_stdout: Arc::new(OutputSequence::new()),
            seq_stderr: Arc::new(OutputSequence::new()),
            callback_panicked: Arc::new(AtomicBool::new(false)),
            publish: Publish::Owned(Arc::new(publish)),
        }
    }

    /// Creates a sink with a synchronous borrowed-chunk callback.
    ///
    /// This constructor lets a composition layer copy a chunk directly into its bounded storage without an intermediate payload allocation.
    /// The callback cannot retain [`OutputChunkRef::line`] after it returns.
    ///
    /// The callback must not block runner execution.
    pub fn new_borrowed<F>(generation: u64, attempt: u32, publish: F) -> Self
    where
        F: for<'a> Fn(OutputChunkRef<'a>) + Send + Sync + 'static,
    {
        Self {
            generation,
            attempt,
            seq_stdout: Arc::new(OutputSequence::new()),
            seq_stderr: Arc::new(OutputSequence::new()),
            callback_panicked: Arc::new(AtomicBool::new(false)),
            publish: Publish::Borrowed(Arc::new(publish)),
        }
    }

    /// Publishes stdout bytes as one or more delimiter-free chunks.
    ///
    /// LF and CRLF delimiters split the input without changing other bytes.
    /// Each emitted chunk receives the next stdout sequence number.
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stdout sequence.
    pub fn stdout_line(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stdout, line, false);
    }

    /// Publishes stdout bytes whose final source line was truncated.
    ///
    /// LF and CRLF delimiters split the input.
    /// Only the final emitted chunk is marked as truncated; every emitted chunk receives its own sequence.
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stdout sequence.
    pub fn stdout_line_truncated(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stdout, line, true);
    }

    /// Publishes stderr bytes as one or more delimiter-free chunks.
    ///
    /// LF and CRLF delimiters split the input without changing other bytes.
    /// Each emitted chunk receives the next stderr sequence number.
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stderr sequence.
    pub fn stderr_line(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stderr, line, false);
    }

    /// Publishes stderr bytes whose final source line was truncated.
    ///
    /// LF and CRLF delimiters split the input.
    /// Only the final emitted chunk is marked as truncated; every emitted chunk receives its own sequence.
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stderr sequence.
    pub fn stderr_line_truncated(&self, line: Bytes) {
        self.push_bytes(StreamKind::Stderr, line, true);
    }

    /// Publishes borrowed stdout bytes as delimiter-free chunks.
    ///
    /// This has the same framing and sequence semantics as [`Self::stdout_line`].
    /// An owned-callback sink copies each emitted chunk once.
    /// A borrowed-callback sink does not allocate payload storage.
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stdout sequence.
    pub fn stdout_line_bytes(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stdout, line, false);
    }

    /// Publishes borrowed stdout bytes whose final source line was truncated.
    ///
    /// This has the same framing, truncation, and sequence semantics as [`Self::stdout_line_truncated`].
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stdout sequence.
    pub fn stdout_line_bytes_truncated(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stdout, line, true);
    }

    /// Publishes borrowed stderr bytes as delimiter-free chunks.
    ///
    /// This has the same framing and sequence semantics as [`Self::stderr_line`].
    /// An owned-callback sink copies each emitted chunk once.
    /// A borrowed-callback sink does not allocate payload storage.
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stderr sequence.
    pub fn stderr_line_bytes(&self, line: &[u8]) {
        self.push_slice(StreamKind::Stderr, line, false);
    }

    /// Publishes borrowed stderr bytes whose final source line was truncated.
    ///
    /// This has the same framing, truncation, and sequence semantics as [`Self::stderr_line_truncated`].
    ///
    /// # Panics
    ///
    /// Panics when an emitted chunk would reuse an exhausted stderr sequence.
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

    /// Returns whether this sink's callback has panicked.
    ///
    /// The state is sticky and shared by every clone.
    /// Once a panic is observed, calls that begin afterward drop their chunks without invoking the callback.
    /// Concurrent calls already in progress may still complete or panic.
    pub fn callback_panicked(&self) -> bool {
        self.callback_panicked.load(Ordering::Acquire)
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
            StreamKind::Stdout => self.seq_stdout.allocate(),
            StreamKind::Stderr => self.seq_stderr.allocate(),
        }
    }

    fn push_owned(&self, stream: StreamKind, line: Bytes, truncated: bool) {
        if self.callback_panicked() {
            return;
        }
        let seq = self.next_seq(stream);
        let ts = SystemTime::now();
        let result = catch_unwind(AssertUnwindSafe(|| match &self.publish {
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
        }));
        if let Err(payload) = result {
            self.handle_callback_panic(payload, stream, seq);
        }
    }

    fn push_borrowed(&self, stream: StreamKind, line: &[u8], truncated: bool) {
        if self.callback_panicked() {
            return;
        }
        let seq = self.next_seq(stream);
        let ts = SystemTime::now();
        let result = catch_unwind(AssertUnwindSafe(|| match &self.publish {
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
        }));
        if let Err(payload) = result {
            self.handle_callback_panic(payload, stream, seq);
        }
    }

    fn handle_callback_panic(&self, payload: PanicPayload, stream: StreamKind, seq: u64) {
        if self.callback_panicked.swap(true, Ordering::AcqRel) {
            dispose_panic_payload(payload);
            return;
        }

        // Dispose the opaque callback payload before invoking application-owned tracing.
        // Neither boundary is allowed to unwind into runner execution.
        dispose_panic_payload(payload);
        let stream = match stream {
            StreamKind::Stdout => "stdout",
            StreamKind::Stderr => "stderr",
        };
        report_without_unwind(|| {
            tracing::error!(
                event = "runner.output_callback_panicked",
                error_kind = "callback_panicked",
                generation = self.generation,
                attempt = self.attempt,
                stream,
                seq,
                "output callback panicked; disabling this attempt sink"
            );
        });
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
    use std::sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;

    use bytes::Bytes;
    use solti_model::{OutputEvent, StreamKind, TaskId};

    use super::{OutputSequence, OutputSink, noop_output_publisher};

    #[test]
    fn output_sequence_issues_max_once_and_then_fails_closed() {
        let sequence = OutputSequence::seeded(u64::MAX - 1);

        assert_eq!(sequence.allocate(), u64::MAX - 1);
        assert_eq!(sequence.allocate(), u64::MAX);
        assert!(
            std::panic::catch_unwind(|| sequence.allocate()).is_err(),
            "sequence exhaustion must panic instead of reusing zero"
        );
    }

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
    fn borrowed_callback_panic_disables_every_sink_clone() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let sink = OutputSink::new_borrowed(7, 3, move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            panic!("borrowed callback panic");
        });
        let clone = sink.clone();

        sink.stdout_line_bytes(b"first");
        clone.stderr_line_bytes(b"second");

        assert!(sink.callback_panicked());
        assert!(clone.callback_panicked());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callback_calls_already_in_progress_can_both_panic() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let entered = Arc::new(Barrier::new(2));
        let callback_entered = Arc::clone(&entered);
        let sink = OutputSink::new(7, 3, move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            callback_entered.wait();
            panic!("concurrent callback panic");
        });
        let stdout = sink.clone();
        let stderr = sink.clone();

        let stdout = thread::spawn(move || stdout.stdout_line_bytes(b"first"));
        let stderr = thread::spawn(move || stderr.stderr_line_bytes(b"second"));
        stdout.join().unwrap();
        stderr.join().unwrap();

        assert!(sink.callback_panicked());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        sink.stdout_line_bytes(b"third");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cross_dispatch_paths_isolate_callback_panics() {
        let owned = OutputSink::new(7, 3, |_| panic!("owned callback panic"));
        owned.stdout_line_bytes(b"borrowed input");
        assert!(owned.callback_panicked());

        let borrowed = OutputSink::new_borrowed(7, 3, |_| panic!("borrowed callback panic"));
        borrowed.stdout_line(Bytes::from_static(b"owned input"));
        assert!(borrowed.callback_panicked());
    }

    #[test]
    fn panicking_panic_payload_drop_cannot_escape_the_sink() {
        struct PanickingDrop;

        impl Drop for PanickingDrop {
            fn drop(&mut self) {
                panic!("panic payload drop");
            }
        }

        let sink = OutputSink::new(7, 3, |_| std::panic::panic_any(PanickingDrop));
        sink.stdout_line_bytes(b"first");

        assert!(sink.callback_panicked());
        sink.stdout_line_bytes(b"second");
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
