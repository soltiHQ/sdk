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
/// This interface has no subscription or lifecycle operations.
pub trait OutputPublisher: Send + Sync {
    /// Returns a sink for one task attempt.
    ///
    /// Returns `None` when output is disabled.
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

/// Write-only output sink for one task attempt.
///
/// Each write creates [`OutputEvent::Chunk`].
/// The chunk uses the sink generation and attempt.
/// Its timestamp comes from [`SystemTime::now`].
///
/// Stdout and stderr have independent sequence counters.
/// Both counters start at `0` and wrap on `u64` overflow.
/// Cloned sinks share those counters.
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
    publish: Arc<dyn Fn(OutputEvent) + Send + Sync>,
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
            publish: Arc::new(publish),
        }
    }

    /// Publishes one stdout chunk.
    pub fn stdout_line(&self, line: Bytes) {
        let seq = self.seq_stdout.fetch_add(1, Ordering::Relaxed);
        self.push(StreamKind::Stdout, seq, line);
    }

    /// Publishes one stderr chunk.
    pub fn stderr_line(&self, line: Bytes) {
        let seq = self.seq_stderr.fetch_add(1, Ordering::Relaxed);
        self.push(StreamKind::Stderr, seq, line);
    }

    /// Returns the attempt number.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the desired-state generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn push(&self, stream: StreamKind, seq: u64, line: Bytes) {
        (self.publish)(OutputEvent::Chunk(OutputChunk {
            generation: self.generation,
            attempt: self.attempt,
            stream,
            seq,
            ts: SystemTime::now(),
            line,
        }));
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
        sink.stderr_line(Bytes::from_static(b"retry"));

        let events = events.lock().unwrap();
        let chunks = events
            .iter()
            .map(|event| match event {
                OutputEvent::Chunk(chunk) => {
                    assert_eq!(chunk.generation, 2);
                    assert_eq!(chunk.attempt, 3);
                    (chunk.stream, chunk.seq, chunk.line.as_ref())
                }
                other => panic!("expected chunk, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            vec![
                (StreamKind::Stdout, 0, b"hello".as_slice()),
                (StreamKind::Stdout, 1, b"again".as_slice()),
                (StreamKind::Stderr, 0, b"oops".as_slice()),
                (StreamKind::Stderr, 1, b"retry".as_slice()),
            ]
        );
        let OutputEvent::Chunk(first_chunk) = &events[0] else {
            panic!("expected chunk");
        };
        assert_eq!(first_chunk.line.as_ptr(), first_pointer);
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
