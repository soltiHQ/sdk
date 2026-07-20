//! Attempt-scoped task output producer capabilities.
//!
//! Runners only produce stdout/stderr chunks. They do not own task output
//! channels, subscriptions, run markers, or terminal cleanup. A composition
//! layer injects an [`OutputPublisher`] through [`BuildContext`](crate::BuildContext),
//! and each runner attempt obtains an [`OutputSink`] from that publisher.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use solti_model::{OutputChunk, OutputEvent, StreamKind, TaskId};

/// Producer capability used by runners to obtain an attempt-scoped output sink.
///
/// Implementations decide whether output is available for a task. Returning
/// `None` disables live output for that attempt without affecting task
/// execution. The trait intentionally exposes no subscription or lifecycle
/// operations.
pub trait OutputPublisher: Send + Sync {
    /// Return a sink for one task attempt, or `None` when output is disabled.
    fn sink_for(&self, task_id: &TaskId, attempt: u32) -> Option<OutputSink>;
}

/// Shared output producer capability injected into runners.
pub type OutputPublisherHandle = Arc<dyn OutputPublisher>;

/// Return an output publisher that disables live output.
pub fn noop_output_publisher() -> OutputPublisherHandle {
    Arc::new(NoOpOutputPublisher)
}

#[derive(Debug)]
struct NoOpOutputPublisher;

impl OutputPublisher for NoOpOutputPublisher {
    fn sink_for(&self, _task_id: &TaskId, _attempt: u32) -> Option<OutputSink> {
        None
    }
}

/// Write-only output sink for one task attempt.
///
/// The sink creates [`OutputEvent::Chunk`] values, assigns independent
/// monotonic sequence numbers to stdout and stderr, and forwards each event to
/// the callback supplied by the publisher implementation. Clones share the
/// same per-stream counters.
#[derive(Clone)]
pub struct OutputSink {
    attempt: u32,
    seq_stdout: Arc<AtomicU64>,
    seq_stderr: Arc<AtomicU64>,
    publish: Arc<dyn Fn(OutputEvent) + Send + Sync>,
}

impl OutputSink {
    /// Build a sink for `attempt` using a synchronous event callback.
    ///
    /// This constructor is intended for [`OutputPublisher`] implementations.
    /// The callback must not block runner execution; live-output transports are
    /// expected to be lossy or otherwise non-blocking.
    pub fn new<F>(attempt: u32, publish: F) -> Self
    where
        F: Fn(OutputEvent) + Send + Sync + 'static,
    {
        Self {
            attempt,
            seq_stdout: Arc::new(AtomicU64::new(0)),
            seq_stderr: Arc::new(AtomicU64::new(0)),
            publish: Arc::new(publish),
        }
    }

    /// Push one stdout line.
    pub fn stdout_line(&self, line: Bytes) {
        let seq = self.seq_stdout.fetch_add(1, Ordering::Relaxed);
        self.push(StreamKind::Stdout, seq, line);
    }

    /// Push one stderr line.
    pub fn stderr_line(&self, line: Bytes) {
        let seq = self.seq_stderr.fetch_add(1, Ordering::Relaxed);
        self.push(StreamKind::Stderr, seq, line);
    }

    /// Return the run attempt this sink belongs to.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    fn push(&self, stream: StreamKind, seq: u64, line: Bytes) {
        (self.publish)(OutputEvent::Chunk(OutputChunk {
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

    fn recording_sink(attempt: u32) -> (OutputSink, Arc<Mutex<Vec<OutputEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let sink = OutputSink::new(attempt, move |event| {
            recorded.lock().unwrap().push(event);
        });
        (sink, events)
    }

    #[test]
    fn sink_publishes_stdout_and_stderr_chunks() {
        let (sink, events) = recording_sink(3);

        sink.stdout_line(Bytes::from_static(b"hello"));
        sink.stderr_line(Bytes::from_static(b"oops"));

        let events = events.lock().unwrap();
        let OutputEvent::Chunk(stdout) = &events[0] else {
            panic!("expected stdout chunk");
        };
        assert_eq!(stdout.attempt, 3);
        assert_eq!(stdout.stream, StreamKind::Stdout);
        assert_eq!(stdout.seq, 0);
        assert_eq!(&stdout.line[..], b"hello");

        let OutputEvent::Chunk(stderr) = &events[1] else {
            panic!("expected stderr chunk");
        };
        assert_eq!(stderr.attempt, 3);
        assert_eq!(stderr.stream, StreamKind::Stderr);
        assert_eq!(stderr.seq, 0);
        assert_eq!(&stderr.line[..], b"oops");
    }

    #[test]
    fn sink_clones_share_monotonic_counters_per_stream() {
        let (sink, events) = recording_sink(1);
        let clone = sink.clone();

        sink.stdout_line(Bytes::from_static(b"a"));
        clone.stdout_line(Bytes::from_static(b"b"));
        clone.stderr_line(Bytes::from_static(b"c"));
        sink.stderr_line(Bytes::from_static(b"d"));

        let events = events.lock().unwrap();
        let chunks = events
            .iter()
            .map(|event| match event {
                OutputEvent::Chunk(chunk) => (chunk.stream, chunk.seq),
                other => panic!("expected chunk, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            chunks,
            vec![
                (StreamKind::Stdout, 0),
                (StreamKind::Stdout, 1),
                (StreamKind::Stderr, 0),
                (StreamKind::Stderr, 1),
            ]
        );
    }

    #[test]
    fn sink_forwards_bytes_without_copying_the_payload() {
        let (sink, events) = recording_sink(1);
        let payload = Bytes::from_static(b"shared");
        let pointer = payload.as_ptr();

        sink.stdout_line(payload);

        let events = events.lock().unwrap();
        let OutputEvent::Chunk(chunk) = &events[0] else {
            panic!("expected chunk");
        };
        assert_eq!(chunk.line.as_ptr(), pointer);
    }

    #[test]
    fn noop_publisher_disables_output() {
        assert!(
            noop_output_publisher()
                .sink_for(&TaskId::from("task-1"), 1)
                .is_none()
        );
    }
}
