//! # Task output broadcast hub.
//!
//! Carries a running task's stdout/stderr (and run-boundary markers) from the runner to any number of live subscribers (a gRPC/SSE log tail, a recorder).
//!
//! ```text
//!   runner attempt ──► OutputSink::stdout_line / stderr_line
//!                            │  (OutputEvent::Chunk)
//!                            ▼
//!                   broadcast::Sender  (one per TaskId, capacity N)
//!                       │        │        │
//!                       ▼        ▼        ▼
//!                    sub #1    sub #2    sub #N      (OutputRegistry::subscribe)
//! ```
//!
//! ## Per-task channel lifecycle (owned by `solti-core`)
//!
//! 1. `ensure_channel` / `sink_for` create the channel - **one [`broadcast::Sender`] per [`TaskId`], reused across every attempt** of that task. A retried task's runs merge into one stream.
//! 2. `subscribe` attaches a receiver; `announce_run_started` / `announce_run_finished` emit the run-boundary markers.
//! 3. `evict` drops the channel once the task is fully terminal (`Exhausted` / `Removed`). The supervisor drives this; the registry never self-reaps.
//!
//! ## Lossy by design
//!
//! Channels are [`tokio::sync::broadcast`] with a fixed ring capacity (see [`OutputRegistry::new`]).
//! A slow subscriber never blocks the runner: it receives a `Lagged` signal and continues from the freshest events still in the ring window.
//! Writes without any live subscriber are dropped silently.
//! The ring is allocated upfront per task, so capacity is a per-task memory vs lag-tolerance trade-off.
//!
//! ## Also
//!
//! - [`BuildContext::output_registry`](crate::BuildContext::output_registry) - how a runner obtains the registry.
//! - [`OutputEvent`](solti_model::OutputEvent) / [`OutputChunk`](solti_model::OutputChunk) - the wire payloads.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use bytes::Bytes;
use parking_lot::RwLock;
use solti_model::{OutputChunk, OutputEvent, StreamKind, TaskId};
use tokio::sync::broadcast;

/// Sink for one task-run attempt.
///
/// Created by [`OutputRegistry::sink_for`] when a runner starts an attempt;
/// the runner pushes lines into it, subscribers (via [`OutputRegistry::subscribe`]) receive [`OutputEvent`]s on the other end.
/// Writes are **lossy** - see the module-level "Lossy by design".
///
/// ## Also
///
/// - [`OutputRegistry`] - owns the channel this sink writes to.
#[derive(Clone)]
pub struct OutputSink {
    attempt: u32,
    seq_stdout: Arc<AtomicU64>,
    seq_stderr: Arc<AtomicU64>,
    sender: broadcast::Sender<OutputEvent>,
}

impl OutputSink {
    /// Build a sink wrapping the given broadcast `Sender`.
    pub fn new(sender: broadcast::Sender<OutputEvent>, attempt: u32) -> Self {
        Self {
            sender,
            attempt,
            seq_stdout: Arc::new(AtomicU64::new(0)),
            seq_stderr: Arc::new(AtomicU64::new(0)),
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

    /// Which run attempt this sink belongs to.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    fn push(&self, stream: StreamKind, seq: u64, line: Bytes) {
        let chunk = OutputChunk {
            attempt: self.attempt,
            stream,
            seq,
            ts: SystemTime::now(),
            line,
        };
        let _ = self.sender.send(OutputEvent::Chunk(chunk));
    }
}

/// Per-task broadcast registry.
///
/// One [`broadcast::Sender`] per [`TaskId`], reused across all attempts of that task, with a fixed ring capacity (see [`OutputRegistry::new`]; [`Default`] uses [`DEFAULT_CAPACITY`](Self::DEFAULT_CAPACITY)).
/// Lifecycle is owned by the supervisor (`solti-core`), see the module-level docs.
/// The registry never self-reaps; the supervisor must call [`evict`](Self::evict).
///
/// ## Also
///
/// - [`OutputSink`] - the write end handed to a runner attempt.
/// - [`BuildContext::output_registry`](crate::BuildContext::output_registry).
///
/// ## Example
///
/// ```rust
/// use solti_runner::OutputRegistry;
/// use solti_model::TaskId;
///
/// let registry = OutputRegistry::new(64);
/// let task = TaskId::from("build-1");
///
/// let _sink = registry.sink_for(task.clone(), 1); // attempt 1 creates the channel
/// assert!(registry.subscribe(&task).is_some());
/// assert_eq!(registry.active_channels(), 1);
///
/// registry.evict(&task);
/// assert!(registry.subscribe(&task).is_none());
/// ```
pub struct OutputRegistry {
    channels: RwLock<HashMap<TaskId, broadcast::Sender<OutputEvent>>>,
    capacity: usize,
}

impl OutputRegistry {
    /// Default per-task ring capacity (in events), used by [`Default`].
    ///
    /// Sized for a live tail: enough to absorb output bursts before a subscriber sees `Lagged`, without the upfront ring allocation dominating per-task memory.
    pub const DEFAULT_CAPACITY: usize = 256;

    /// Build an empty registry.
    ///
    /// `capacity` is the ring size of every per-task broadcast channel, allocated upfront when the channel is created (even with zero subscribers).
    /// Larger values let slower subscribers fall further behind before `Lagged`; smaller values cut the fixed per-task memory cost.
    pub fn new(capacity: usize) -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            capacity,
        }
    }

    /// Pre-create the broadcast channel for `task_id` without producing a sink.
    ///
    /// No-op if the channel already exists.
    pub fn ensure_channel(&self, task_id: TaskId) {
        let mut channels = self.channels.write();
        channels
            .entry(task_id)
            .or_insert_with(|| broadcast::channel::<OutputEvent>(self.capacity).0);
    }

    /// Get an [`OutputSink`] for `(task_id, attempt)`.
    /// The first call for a given `task_id` creates the broadcast channel; subsequent calls reuse it (multi-run merge).
    /// The returned sink has fresh per-stream `seq` counters scoped to this attempt.
    pub fn sink_for(&self, task_id: TaskId, attempt: u32) -> OutputSink {
        let mut channels = self.channels.write();
        let sender = channels
            .entry(task_id)
            .or_insert_with(|| broadcast::channel::<OutputEvent>(self.capacity).0)
            .clone();
        OutputSink::new(sender, attempt)
    }

    /// Subscribe to a task's output stream.
    pub fn subscribe(&self, task_id: &TaskId) -> Option<broadcast::Receiver<OutputEvent>> {
        let channels = self.channels.read();
        channels.get(task_id).map(|s| s.subscribe())
    }

    /// Push a [`OutputEvent::RunStarted`] into the channel.
    ///
    /// No-op if no channel exists for this task.
    pub fn announce_run_started(&self, task_id: &TaskId, attempt: u32) {
        let channels = self.channels.read();
        if let Some(sender) = channels.get(task_id) {
            let _ = sender.send(OutputEvent::RunStarted {
                attempt,
                started_at: SystemTime::now(),
            });
        }
    }

    /// Push a [`OutputEvent::RunFinished`] into the channel.
    ///
    /// No-op if no channel exists for this task.
    pub fn announce_run_finished(&self, task_id: &TaskId, attempt: u32, exit_code: Option<i32>) {
        let channels = self.channels.read();
        if let Some(sender) = channels.get(task_id) {
            let _ = sender.send(OutputEvent::RunFinished {
                attempt,
                exit_code,
                finished_at: SystemTime::now(),
            });
        }
    }

    /// Drop the broadcast channel for `task_id`.
    pub fn evict(&self, task_id: &TaskId) {
        let mut channels = self.channels.write();
        channels.remove(task_id);
    }

    /// Number of tasks with an active channel.
    pub fn active_channels(&self) -> usize {
        self.channels.read().len()
    }
}

impl Default for OutputRegistry {
    /// Build a registry with [`DEFAULT_CAPACITY`](Self::DEFAULT_CAPACITY) events per task.
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use solti_model::{OutputEvent, StreamKind, TaskId};
    use tokio::sync::broadcast;

    use super::{OutputRegistry, OutputSink};

    #[tokio::test]
    async fn output_sink_pushes_stdout_line_to_subscriber() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = OutputSink::new(tx, 1);

        sink.stdout_line(Bytes::from_static(b"hello"));

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(chunk) => {
                assert_eq!(chunk.attempt, 1);
                assert_eq!(chunk.stream, StreamKind::Stdout);
                assert_eq!(chunk.seq, 0);
                assert_eq!(&chunk.line[..], b"hello");
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_sink_pushes_stderr_line_to_subscriber() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = OutputSink::new(tx, 5);

        sink.stderr_line(Bytes::from_static(b"oops"));

        match rx.recv().await.unwrap() {
            OutputEvent::Chunk(chunk) => {
                assert_eq!(chunk.attempt, 5);
                assert_eq!(chunk.stream, StreamKind::Stderr);
                assert_eq!(&chunk.line[..], b"oops");
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_sink_assigns_monotonic_seq_per_stream() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = OutputSink::new(tx, 1);

        sink.stdout_line(Bytes::from_static(b"a"));
        sink.stdout_line(Bytes::from_static(b"b"));
        sink.stdout_line(Bytes::from_static(b"c"));

        let mut seqs = Vec::new();
        for _ in 0..3 {
            if let OutputEvent::Chunk(c) = rx.recv().await.unwrap() {
                seqs.push(c.seq);
            }
        }
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn output_sink_seq_is_independent_per_stream() {
        let (tx, mut rx) = broadcast::channel::<OutputEvent>(16);
        let sink = OutputSink::new(tx, 1);

        sink.stdout_line(Bytes::from_static(b"o1"));
        sink.stderr_line(Bytes::from_static(b"e1"));
        sink.stdout_line(Bytes::from_static(b"o2"));
        sink.stderr_line(Bytes::from_static(b"e2"));

        let mut stdout_seqs = Vec::new();
        let mut stderr_seqs = Vec::new();
        for _ in 0..4 {
            if let OutputEvent::Chunk(c) = rx.recv().await.unwrap() {
                match c.stream {
                    StreamKind::Stdout => stdout_seqs.push(c.seq),
                    StreamKind::Stderr => stderr_seqs.push(c.seq),
                }
            }
        }
        assert_eq!(stdout_seqs, vec![0, 1]);
        assert_eq!(stderr_seqs, vec![0, 1]);
    }

    #[tokio::test]
    async fn output_sink_does_not_panic_without_subscribers() {
        let (tx, _) = broadcast::channel::<OutputEvent>(16);
        let sink = OutputSink::new(tx, 1);

        sink.stdout_line(Bytes::from_static(b"nobody-listens"));
        sink.stderr_line(Bytes::from_static(b"still-no-one"));
    }

    #[tokio::test]
    async fn output_sink_fans_out_to_multiple_subscribers() {
        let (tx, mut rx1) = broadcast::channel::<OutputEvent>(16);
        let mut rx2 = tx.subscribe();
        let sink = OutputSink::new(tx, 2);

        sink.stdout_line(Bytes::from_static(b"hello"));

        for rx in [&mut rx1, &mut rx2] {
            if let OutputEvent::Chunk(c) = rx.recv().await.unwrap() {
                assert_eq!(&c.line[..], b"hello");
            } else {
                panic!("expected Chunk");
            }
        }
    }

    #[tokio::test]
    async fn output_sink_forwards_line_to_subscribers_without_byte_copy() {
        let (tx, mut rx1) = broadcast::channel::<OutputEvent>(16);
        let mut rx2 = tx.subscribe();
        let sink = OutputSink::new(tx, 1);

        let payload = Bytes::from_static(b"shared-line");
        let payload_ptr = payload.as_ptr();
        sink.stdout_line(payload);

        for rx in [&mut rx1, &mut rx2] {
            if let OutputEvent::Chunk(c) = rx.recv().await.unwrap() {
                assert_eq!(
                    c.line.as_ptr(),
                    payload_ptr,
                    "line bytes must be shared across subscribers"
                );
            } else {
                panic!("expected Chunk");
            }
        }
    }

    #[tokio::test]
    async fn registry_subscribe_returns_none_before_first_sink_for() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-1");
        assert!(reg.subscribe(&task).is_none());
    }

    #[tokio::test]
    async fn registry_subscribe_returns_some_after_sink_for() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-2");
        let _sink = reg.sink_for(task.clone(), 1);
        assert!(reg.subscribe(&task).is_some());
    }

    #[tokio::test]
    async fn registry_sink_for_reuses_channel_across_attempts() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-merge");

        let sink_a1 = reg.sink_for(task.clone(), 1);
        let mut rx = reg.subscribe(&task).unwrap();

        sink_a1.stdout_line(Bytes::from_static(b"from-attempt-1"));

        let sink_a2 = reg.sink_for(task.clone(), 2);
        sink_a2.stdout_line(Bytes::from_static(b"from-attempt-2"));

        let mut seen = Vec::new();
        for _ in 0..2 {
            if let OutputEvent::Chunk(c) = rx.recv().await.unwrap() {
                seen.push((c.attempt, std::str::from_utf8(&c.line).unwrap().to_string()));
            }
        }
        assert_eq!(
            seen,
            vec![
                (1u32, "from-attempt-1".to_string()),
                (2u32, "from-attempt-2".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn registry_announce_run_started_emits_boundary_event() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-bound");
        let _sink = reg.sink_for(task.clone(), 1);
        let mut rx = reg.subscribe(&task).unwrap();

        reg.announce_run_started(&task, 1);

        match rx.recv().await.unwrap() {
            OutputEvent::RunStarted { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected RunStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_announce_run_finished_carries_exit_code() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-fin");
        let _sink = reg.sink_for(task.clone(), 3);
        let mut rx = reg.subscribe(&task).unwrap();

        reg.announce_run_finished(&task, 3, Some(0));

        match rx.recv().await.unwrap() {
            OutputEvent::RunFinished {
                attempt, exit_code, ..
            } => {
                assert_eq!(attempt, 3);
                assert_eq!(exit_code, Some(0));
            }
            other => panic!("expected RunFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registry_evict_drops_channel() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-evict");
        let _sink = reg.sink_for(task.clone(), 1);
        assert!(reg.subscribe(&task).is_some());

        reg.evict(&task);
        assert!(reg.subscribe(&task).is_none());
    }

    #[tokio::test]
    async fn registry_active_channels_reflects_state() {
        let reg = OutputRegistry::new(16);
        assert_eq!(reg.active_channels(), 0);

        let _ = reg.sink_for(TaskId::from("a"), 1);
        let _ = reg.sink_for(TaskId::from("b"), 1);
        assert_eq!(reg.active_channels(), 2);

        reg.evict(&TaskId::from("a"));
        assert_eq!(reg.active_channels(), 1);
    }

    #[tokio::test]
    async fn registry_ensure_channel_creates_subscribable_channel() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-ensure");
        assert!(reg.subscribe(&task).is_none());

        reg.ensure_channel(task.clone());
        assert!(reg.subscribe(&task).is_some());
    }

    #[tokio::test]
    async fn registry_ensure_channel_is_idempotent() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-idem");

        reg.ensure_channel(task.clone());
        let mut rx = reg.subscribe(&task).unwrap();

        reg.ensure_channel(task.clone());

        let _ = reg.sink_for(task.clone(), 1);
        let _ = reg.subscribe(&task).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn registry_default_uses_default_capacity() {
        let reg = OutputRegistry::default();
        assert_eq!(reg.capacity, OutputRegistry::DEFAULT_CAPACITY);
        assert_eq!(OutputRegistry::DEFAULT_CAPACITY, 256);
    }

    #[tokio::test]
    async fn registry_announce_without_channel_is_noop() {
        let reg = OutputRegistry::new(16);
        let task = TaskId::from("t-ghost");

        reg.announce_run_started(&task, 1);
        reg.announce_run_finished(&task, 1, None);

        assert!(
            reg.subscribe(&task).is_none(),
            "must not auto-create channel"
        );
    }
}
