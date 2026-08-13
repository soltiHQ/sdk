//! # Live task output
//!
//! Core owns task output channels.
//! Runners receive only the publishing side.
//! Consumers receive only [`OutputSubscription`].
//!
//! ## Flow
//!
//! ```text
//! Runner
//!    │ OutputSink
//!    ▼
//! single ownership copy + chunk limit
//!    ▼
//! per-task byte-bounded broadcast ring
//!    │
//!    ▼
//! OutputSubscription
//! ```
//!
//! Output is live-only and best-effort.
//! It is not stored in task history.
//! Oversized chunks are exact prefixes with `truncated = true`.
//! Slow subscribers receive [`OutputEvent::Lagged`] with skipped event and
//! retained-payload byte counts.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::SystemTime;

use bytes::Bytes;
use parking_lot::RwLock;
use solti_model::{OutputChunk, OutputEvent, TaskId, Uid};
use solti_runner::{OutputChunkRef, OutputPublisher, OutputSink};
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::ConfigError;
use crate::persistence::{TaskOutputEvent, TaskOutputSinkHandle, publish_output_event};

/// Per-task live output settings.
///
/// Event count and retained chunk bytes are bounded independently.
/// The broadcast ring uses the stricter of both limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    capacity: NonZeroUsize,
    byte_budget: NonZeroUsize,
    max_chunk_bytes: NonZeroUsize,
}

impl OutputConfig {
    /// Default per-task event capacity.
    pub const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();
    /// Default maximum bytes in one output chunk.
    pub const DEFAULT_MAX_CHUNK_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
    /// Default per-task retained chunk payload budget.
    pub const DEFAULT_BYTE_BUDGET: NonZeroUsize = NonZeroUsize::new(16 * 1024 * 1024).unwrap();

    /// Creates settings with a non-zero event capacity.
    ///
    /// The byte budget scales with `capacity` using
    /// [`Self::DEFAULT_MAX_CHUNK_BYTES`]. Saturation can make
    /// [`Self::effective_capacity`] lower than `capacity`.
    pub const fn new(capacity: NonZeroUsize) -> Self {
        let byte_budget = capacity
            .get()
            .saturating_mul(Self::DEFAULT_MAX_CHUNK_BYTES.get());
        Self {
            capacity,
            byte_budget: NonZeroUsize::new(byte_budget).unwrap(),
            max_chunk_bytes: Self::DEFAULT_MAX_CHUNK_BYTES,
        }
    }

    /// Creates settings from a raw event capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `capacity` is zero.
    pub const fn try_new(capacity: usize) -> Result<Self, ConfigError> {
        let Some(capacity) = NonZeroUsize::new(capacity) else {
            return Err(ConfigError::Zero {
                field: "output_capacity",
            });
        };
        Ok(Self::new(capacity))
    }

    /// Returns the configured per-task event capacity.
    pub const fn capacity(self) -> NonZeroUsize {
        self.capacity
    }

    /// Returns the per-task retained chunk payload budget.
    ///
    /// Event metadata is separately bounded by [`Self::capacity`].
    pub const fn byte_budget(self) -> NonZeroUsize {
        self.byte_budget
    }

    /// Returns the largest output chunk retained from a runner.
    ///
    /// Larger chunks become exact prefixes marked as truncated.
    pub const fn max_chunk_bytes(self) -> NonZeroUsize {
        self.max_chunk_bytes
    }

    /// Returns the broadcast ring capacity after applying both limits.
    ///
    /// Tokio rounds broadcast capacities up to a power of two. This method
    /// rounds down first so the allocated ring cannot exceed either limit.
    pub const fn effective_capacity(self) -> NonZeroUsize {
        let byte_capacity = self.byte_budget.get() / self.max_chunk_bytes.get();
        let upper_bound = if self.capacity.get() < byte_capacity {
            self.capacity.get()
        } else {
            byte_capacity
        };
        let upper_bound = if upper_bound < (usize::MAX >> 1) {
            upper_bound
        } else {
            usize::MAX >> 1
        };
        let mut capacity = 1;
        while capacity <= upper_bound / 2 {
            capacity *= 2;
        }
        NonZeroUsize::new(capacity).unwrap()
    }

    /// Sets the retained chunk payload budget and maximum chunk size together.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when either value is zero.
    /// Returns [`ConfigError::Exceeds`] when `max_chunk_bytes` exceeds
    /// `byte_budget`.
    pub const fn try_with_byte_limits(
        mut self,
        byte_budget: usize,
        max_chunk_bytes: usize,
    ) -> Result<Self, ConfigError> {
        let Some(byte_budget) = NonZeroUsize::new(byte_budget) else {
            return Err(ConfigError::Zero {
                field: "output_byte_budget",
            });
        };
        let Some(max_chunk_bytes) = NonZeroUsize::new(max_chunk_bytes) else {
            return Err(ConfigError::Zero {
                field: "output_max_chunk_bytes",
            });
        };
        if max_chunk_bytes.get() > byte_budget.get() {
            return Err(ConfigError::Exceeds {
                field: "output_max_chunk_bytes",
                limit: "output_byte_budget",
            });
        }
        self.byte_budget = byte_budget;
        self.max_chunk_bytes = max_chunk_bytes;
        Ok(self)
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

/// Live stream of one task's output events.
///
/// The stream is lossy.
/// A slow consumer receives [`OutputEvent::Lagged`].
/// It then continues with newer events.
///
/// Terminal cleanup prevents new subscriptions.
/// An existing subscription closes after every runner sink releases its sender.
///
/// The stream implements [`tokio_stream::Stream`].
/// Its item type is [`OutputEvent`].
pub struct OutputSubscription {
    inner: BroadcastStream<BroadcastOutput>,
    total_bytes: Arc<AtomicU64>,
    next_bytes: u64,
    pending_lag: u64,
    pending_event: Option<OutputEvent>,
}

impl OutputSubscription {
    fn new(
        receiver: broadcast::Receiver<BroadcastOutput>,
        total_bytes: Arc<AtomicU64>,
        next_bytes: u64,
    ) -> Self {
        Self {
            inner: BroadcastStream::new(receiver),
            total_bytes,
            next_bytes,
            pending_lag: 0,
            pending_event: None,
        }
    }
}

impl Stream for OutputSubscription {
    type Item = OutputEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(event) = this.pending_event.take() {
            return Poll::Ready(Some(event));
        }

        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(output))) => {
                    if this.pending_lag != 0 {
                        let skipped = std::mem::take(&mut this.pending_lag);
                        let skipped_bytes = output.bytes_before.saturating_sub(this.next_bytes);
                        this.next_bytes = output.bytes_after;
                        this.pending_event = Some(output.event);
                        return Poll::Ready(Some(OutputEvent::Lagged {
                            skipped,
                            skipped_bytes,
                        }));
                    }
                    this.next_bytes = output.bytes_after;
                    return Poll::Ready(Some(output.event));
                }
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(skipped)))) => {
                    this.pending_lag = this.pending_lag.saturating_add(skipped);
                }
                Poll::Ready(None) if this.pending_lag != 0 => {
                    let skipped = std::mem::take(&mut this.pending_lag);
                    let skipped_bytes = this
                        .total_bytes
                        .load(Ordering::Acquire)
                        .saturating_sub(this.next_bytes);
                    return Poll::Ready(Some(OutputEvent::Lagged {
                        skipped,
                        skipped_bytes,
                    }));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[derive(Clone)]
struct BroadcastOutput {
    event: OutputEvent,
    bytes_before: u64,
    bytes_after: u64,
}

struct OutputBroadcaster {
    state: parking_lot::Mutex<BroadcastState>,
    total_bytes: Arc<AtomicU64>,
}

struct BroadcastState {
    sender: broadcast::Sender<BroadcastOutput>,
    total_bytes: u64,
}

impl OutputBroadcaster {
    fn new(capacity: usize) -> Self {
        Self {
            state: parking_lot::Mutex::new(BroadcastState {
                sender: broadcast::channel(capacity).0,
                total_bytes: 0,
            }),
            total_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    fn subscribe(&self) -> OutputSubscription {
        let state = self.state.lock();
        OutputSubscription::new(
            state.sender.subscribe(),
            Arc::clone(&self.total_bytes),
            state.total_bytes,
        )
    }

    fn send(&self, event: OutputEvent) {
        let mut state = self.state.lock();
        let bytes_before = state.total_bytes;
        let bytes_after = bytes_before.saturating_add(output_payload_bytes(&event));
        state.total_bytes = bytes_after;
        self.total_bytes.store(bytes_after, Ordering::Release);
        let _ = state.sender.send(BroadcastOutput {
            event,
            bytes_before,
            bytes_after,
        });
    }

    #[cfg(test)]
    fn subscribe_raw(&self) -> RawOutputReceiver {
        RawOutputReceiver(self.state.lock().sender.subscribe())
    }
}

fn output_payload_bytes(event: &OutputEvent) -> u64 {
    match event {
        OutputEvent::Chunk(chunk) => u64::try_from(chunk.line.len()).unwrap_or(u64::MAX),
        _ => 0,
    }
}

#[cfg(test)]
pub(crate) struct RawOutputReceiver(broadcast::Receiver<BroadcastOutput>);

#[cfg(test)]
impl RawOutputReceiver {
    pub(crate) fn try_recv(&mut self) -> Result<OutputEvent, broadcast::error::TryRecvError> {
        self.0.try_recv().map(|output| output.event)
    }
}

/// Core-owned output channel registry.
pub(crate) struct OutputHub {
    channels: RwLock<HashMap<TaskId, OutputChannel>>,
    capacity: usize,
    max_chunk_bytes: usize,
    event_sink: Option<TaskOutputSinkHandle>,
}

struct OutputChannel {
    task_uid: Uid,
    broadcaster: Arc<OutputBroadcaster>,
}

impl OutputHub {
    #[cfg(test)]
    pub(crate) fn new(config: OutputConfig) -> Self {
        Self::with_sink(config, None)
    }

    pub(crate) fn with_sink(
        config: OutputConfig,
        event_sink: Option<TaskOutputSinkHandle>,
    ) -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            capacity: config.effective_capacity().get(),
            max_chunk_bytes: config.max_chunk_bytes().get(),
            event_sink,
        }
    }

    /// Ensures that a task channel exists.
    ///
    /// Returns `true` when this call creates it.
    pub(crate) fn ensure_channel_if_absent(&self, task_id: TaskId, task_uid: Uid) -> bool {
        let mut channels = self.channels.write();
        match channels.entry(task_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(OutputChannel {
                    task_uid,
                    broadcaster: Arc::new(OutputBroadcaster::new(self.capacity)),
                });
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn ensure_channel(&self, task_id: TaskId) -> Uid {
        let task_uid = Uid::new(format!("test-{task_id}")).expect("test UID");
        self.ensure_channel_if_absent(task_id, task_uid.clone());
        task_uid
    }

    pub(crate) fn subscribe(&self, task_id: &TaskId) -> Option<OutputSubscription> {
        self.channels
            .read()
            .get(task_id)
            .map(|channel| channel.broadcaster.subscribe())
    }

    #[cfg(test)]
    pub(crate) fn subscribe_raw(&self, task_id: &TaskId) -> Option<RawOutputReceiver> {
        self.channels
            .read()
            .get(task_id)
            .map(|channel| channel.broadcaster.subscribe_raw())
    }

    pub(crate) fn announce_run_started(
        &self,
        task_id: &TaskId,
        task_uid: &Uid,
        generation: u64,
        attempt: u32,
    ) {
        self.send_lifecycle(
            task_id,
            task_uid,
            OutputEvent::RunStarted {
                generation,
                attempt,
                started_at: SystemTime::now(),
            },
        );
    }

    pub(crate) fn announce_run_finished(
        &self,
        task_id: &TaskId,
        task_uid: &Uid,
        generation: u64,
        attempt: u32,
        exit_code: Option<i32>,
    ) {
        self.send_lifecycle(
            task_id,
            task_uid,
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                finished_at: SystemTime::now(),
            },
        );
    }

    fn send_lifecycle(&self, task_id: &TaskId, task_uid: &Uid, event: OutputEvent) {
        debug_assert!(!matches!(&event, OutputEvent::Chunk(_)));
        if let Some(broadcaster) = self
            .channels
            .read()
            .get(task_id)
            .map(|channel| Arc::clone(&channel.broadcaster))
        {
            broadcaster.send(event.clone());
        }
        publish_output_event(
            self.event_sink.as_ref(),
            TaskOutputEvent::new(task_id.clone(), task_uid.clone(), event),
        );
    }

    pub(crate) fn evict(&self, task_id: &TaskId) {
        self.channels.write().remove(task_id);
    }

    #[cfg(test)]
    pub(crate) fn active_channels(&self) -> usize {
        self.channels.read().len()
    }
}

impl OutputPublisher for OutputHub {
    fn sink_for(&self, task_id: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        let channels = self.channels.read();
        let channel = channels.get(task_id)?;
        let broadcaster = Arc::clone(&channel.broadcaster);
        let task_uid = channel.task_uid.clone();
        drop(channels);
        let event_sink = self.event_sink.clone();
        let max_chunk_bytes = self.max_chunk_bytes;
        let task_id = task_id.clone();
        Some(OutputSink::new_borrowed(
            generation,
            attempt,
            move |chunk| {
                let event = detach_chunk(chunk, max_chunk_bytes);
                broadcaster.send(event.clone());
                publish_output_event(
                    event_sink.as_ref(),
                    TaskOutputEvent::new(task_id.clone(), task_uid.clone(), event),
                );
            },
        ))
    }
}

fn detach_chunk(chunk: OutputChunkRef<'_>, max_chunk_bytes: usize) -> OutputEvent {
    let retained_len = chunk.line().len().min(max_chunk_bytes);
    let truncated = chunk.truncated() || chunk.line().len() > retained_len;
    // The runner-facing view is borrowed. This single copy both detaches an
    // arbitrary producer allocation and enforces the ring's backing bound.
    OutputEvent::Chunk(OutputChunk {
        generation: chunk.generation(),
        attempt: chunk.attempt(),
        stream: chunk.stream(),
        seq: chunk.seq(),
        ts: chunk.timestamp(),
        line: Bytes::copy_from_slice(&chunk.line()[..retained_len]),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use solti_model::{OutputEvent, TaskId, Uid};
    use solti_runner::OutputPublisher;
    use tokio_stream::StreamExt;

    use super::{ConfigError, OutputConfig, OutputHub};
    use crate::{TaskOutputEvent, TaskOutputSink, TaskOutputSinkHandle};

    #[derive(Default)]
    struct RecordingOutputSink {
        events: Mutex<Vec<TaskOutputEvent>>,
    }

    impl TaskOutputSink for RecordingOutputSink {
        fn on_event(&self, event: &TaskOutputEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    #[test]
    fn config_preserves_default_and_rejects_zero() {
        let default = OutputConfig::default();
        assert_eq!(default.capacity(), OutputConfig::DEFAULT_CAPACITY);
        assert_eq!(default.effective_capacity(), OutputConfig::DEFAULT_CAPACITY);
        assert_eq!(default.byte_budget(), OutputConfig::DEFAULT_BYTE_BUDGET);
        assert_eq!(
            default.max_chunk_bytes(),
            OutputConfig::DEFAULT_MAX_CHUNK_BYTES
        );
        assert_eq!(OutputConfig::DEFAULT_CAPACITY.get(), 256);
        assert_eq!(OutputConfig::DEFAULT_BYTE_BUDGET.get(), 16 * 1024 * 1024);
        assert_eq!(OutputConfig::DEFAULT_MAX_CHUNK_BYTES.get(), 64 * 1024);
        assert_eq!(
            OutputConfig::try_new(0).unwrap_err(),
            ConfigError::Zero {
                field: "output_capacity"
            }
        );
        assert_eq!(OutputConfig::try_new(64).unwrap().capacity().get(), 64);
    }

    #[test]
    fn byte_limits_are_checked_and_bound_the_ring() {
        let config = OutputConfig::try_new(8)
            .unwrap()
            .try_with_byte_limits(8, 4)
            .unwrap();
        assert_eq!(config.capacity().get(), 8);
        assert_eq!(config.effective_capacity().get(), 2);
        assert_eq!(config.byte_budget().get(), 8);
        assert_eq!(config.max_chunk_bytes().get(), 4);

        let rounded = OutputConfig::try_new(8)
            .unwrap()
            .try_with_byte_limits(12, 4)
            .unwrap();
        assert_eq!(
            rounded.effective_capacity().get(),
            2,
            "Tokio must not round a capacity of three up past the byte budget"
        );

        assert_eq!(
            OutputConfig::default()
                .try_with_byte_limits(0, 1)
                .unwrap_err(),
            ConfigError::Zero {
                field: "output_byte_budget"
            }
        );
        assert_eq!(
            OutputConfig::default()
                .try_with_byte_limits(1, 0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "output_max_chunk_bytes"
            }
        );
        assert_eq!(
            OutputConfig::default()
                .try_with_byte_limits(3, 4)
                .unwrap_err(),
            ConfigError::Exceeds {
                field: "output_max_chunk_bytes",
                limit: "output_byte_budget"
            }
        );
    }

    #[test]
    fn producer_cannot_create_a_task_channel() {
        let hub = OutputHub::new(OutputConfig::default());
        let task_id = TaskId::new("missing").unwrap();

        assert!(hub.sink_for(&task_id, 1, 1).is_none());
        assert_eq!(hub.active_channels(), 0);
    }

    #[tokio::test]
    async fn attempts_share_one_stream_with_run_markers() {
        let hub = OutputHub::new(OutputConfig::try_new(16).unwrap());
        let task_id = TaskId::new("retrying").unwrap();
        let task_uid = hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");

        hub.announce_run_started(&task_id, &task_uid, 1, 1);
        hub.sink_for(&task_id, 1, 1)
            .expect("attempt one sink")
            .stdout_line(Bytes::from_static(b"one"));
        hub.announce_run_finished(&task_id, &task_uid, 1, 1, Some(1));
        hub.announce_run_started(&task_id, &task_uid, 1, 2);
        hub.sink_for(&task_id, 1, 2)
            .expect("attempt two sink")
            .stderr_line(Bytes::from_static(b"two"));

        assert!(matches!(
            output.next().await,
            Some(OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                ..
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if chunk.generation == 1 && chunk.attempt == 1 && &chunk.line[..] == b"one"
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(1),
                ..
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::RunStarted {
                generation: 1,
                attempt: 2,
                ..
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if chunk.generation == 1 && chunk.attempt == 2 && &chunk.line[..] == b"two"
        ));
    }

    #[test]
    fn external_sink_receives_run_markers_and_first_chunk() {
        let recording = Arc::new(RecordingOutputSink::default());
        let sink: TaskOutputSinkHandle = recording.clone();
        let hub = OutputHub::with_sink(OutputConfig::try_new(16).unwrap(), Some(sink));
        let task_id = TaskId::new("persisted-output").unwrap();
        let task_uid = hub.ensure_channel(task_id.clone());

        hub.announce_run_started(&task_id, &task_uid, 1, 1);
        hub.sink_for(&task_id, 1, 1)
            .expect("attempt sink")
            .stdout_line(Bytes::from_static(b"first"));
        hub.announce_run_finished(&task_id, &task_uid, 1, 1, Some(0));

        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.task() == &task_id));
        assert!(events.iter().all(|event| event.task_uid() == &task_uid));
        assert!(matches!(
            events[0].event(),
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                ..
            }
        ));
        assert!(matches!(
            events[1].event(),
            OutputEvent::Chunk(chunk) if &chunk.line[..] == b"first"
        ));
        assert!(matches!(
            events[2].event(),
            OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(0),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn runner_chunks_are_detached_and_bounded_before_delivery() {
        let recording = Arc::new(RecordingOutputSink::default());
        let persistence: TaskOutputSinkHandle = recording.clone();
        let config = OutputConfig::try_new(8)
            .unwrap()
            .try_with_byte_limits(16, 4)
            .unwrap();
        let hub = OutputHub::with_sink(config, Some(persistence));
        let task_id = TaskId::new("bounded-output").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("attempt sink");
        let source = Bytes::from_static(b"abcdefgh");
        let source_ptr = source.as_ptr();
        let backing = Bytes::from(vec![b'z'; 1024]);
        let small_view = backing.slice(100..104);
        let small_view_ptr = small_view.as_ptr();

        sink.stdout_line(source);
        sink.stderr_line(small_view);

        let live = output.next().await.expect("live output");
        let OutputEvent::Chunk(live) = live else {
            panic!("expected live chunk");
        };
        assert_eq!(&live.line[..], b"abcd");
        assert!(live.truncated);
        assert_ne!(
            live.line.as_ptr(),
            source_ptr,
            "a retained prefix must not keep the oversized allocation"
        );

        let small = output.next().await.expect("small live output");
        let OutputEvent::Chunk(small) = small else {
            panic!("expected small live chunk");
        };
        assert_eq!(&small.line[..], b"zzzz");
        assert!(!small.truncated);
        assert_ne!(
            small.line.as_ptr(),
            small_view_ptr,
            "a short Bytes view must not keep a large backing allocation"
        );

        let events = recording.events.lock().unwrap();
        let OutputEvent::Chunk(persisted) = events[0].event() else {
            panic!("expected persisted chunk");
        };
        assert_eq!(&persisted.line[..], b"abcd");
        assert!(persisted.truncated);
        assert_eq!(
            persisted.line.as_ptr(),
            live.line.as_ptr(),
            "live delivery and persistence must share the one detached payload"
        );
        let OutputEvent::Chunk(persisted_small) = events[1].event() else {
            panic!("expected persisted small chunk");
        };
        assert_eq!(&persisted_small.line[..], b"zzzz");
        assert!(!persisted_small.truncated);
        assert_eq!(
            persisted_small.line.as_ptr(),
            small.line.as_ptr(),
            "live delivery and persistence must share the one detached payload"
        );
    }

    #[test]
    fn stale_output_keeps_the_original_task_uid() {
        let recording = Arc::new(RecordingOutputSink::default());
        let sink: TaskOutputSinkHandle = recording.clone();
        let hub = OutputHub::with_sink(OutputConfig::try_new(16).unwrap(), Some(sink));
        let task_id = TaskId::new("recreated-output").unwrap();
        let old_uid = Uid::new("old-output-incarnation").unwrap();
        let new_uid = Uid::new("new-output-incarnation").unwrap();

        assert!(hub.ensure_channel_if_absent(task_id.clone(), old_uid.clone()));
        let stale_sink = hub.sink_for(&task_id, 1, 1).expect("old sink");
        hub.evict(&task_id);
        assert!(hub.ensure_channel_if_absent(task_id.clone(), new_uid.clone()));
        let current_sink = hub.sink_for(&task_id, 1, 1).expect("new sink");

        stale_sink.stdout_line(Bytes::from_static(b"old"));
        current_sink.stdout_line(Bytes::from_static(b"new"));

        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].task_uid(), &old_uid);
        assert_eq!(events[1].task_uid(), &new_uid);
    }

    #[tokio::test]
    async fn subscription_reports_lag_and_continues() {
        let hub = OutputHub::new(OutputConfig::try_new(1).unwrap());
        let task_id = TaskId::new("lagged").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("sink");

        sink.stdout_line(Bytes::from_static(b"one"));
        sink.stdout_line(Bytes::from_static(b"two"));

        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Lagged {
                skipped,
                skipped_bytes: 3,
            }) if skipped > 0
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"two"
        ));
    }

    #[tokio::test]
    async fn byte_budget_reduces_capacity_and_reports_lag() {
        let config = OutputConfig::try_new(8)
            .unwrap()
            .try_with_byte_limits(8, 4)
            .unwrap();
        let hub = OutputHub::new(config);
        let task_id = TaskId::new("byte-budget-lagged").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("sink");

        sink.stdout_line(Bytes::from_static(b"1111"));
        sink.stdout_line(Bytes::from_static(b"2222"));
        sink.stdout_line(Bytes::from_static(b"3333"));

        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Lagged {
                skipped: 1,
                skipped_bytes: 4,
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"2222"
        ));
    }

    #[tokio::test]
    async fn terminal_evict_waits_for_outstanding_sink_clones() {
        let hub = OutputHub::new(OutputConfig::try_new(8).unwrap());
        let task_id = TaskId::new("closing").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("sink");
        let outstanding = sink.clone();

        hub.evict(&task_id);
        assert!(hub.subscribe(&task_id).is_none());
        sink.stdout_line(Bytes::from_static(b"after-evict"));
        assert!(matches!(output.next().await, Some(OutputEvent::Chunk(_))));

        drop(sink);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), output.next())
                .await
                .is_err(),
            "the outstanding clone still owns the sender"
        );
        drop(outstanding);
        assert!(output.next().await.is_none());
    }

    #[tokio::test]
    async fn stale_sink_cannot_publish_into_a_reused_task_id() {
        let hub = Arc::new(OutputHub::new(OutputConfig::try_new(8).unwrap()));
        let task_id = TaskId::new("reused").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut old_output = hub.subscribe(&task_id).expect("old subscription");
        let stale_sink = hub.sink_for(&task_id, 1, 1).expect("old sink");

        hub.evict(&task_id);
        hub.ensure_channel(task_id.clone());
        let mut new_output = hub.subscribe(&task_id).expect("new subscription");

        stale_sink.stdout_line(Bytes::from_static(b"stale"));
        assert!(matches!(
            old_output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"stale"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), new_output.next())
                .await
                .is_err(),
            "a stale sink must remain attached to the old generation"
        );
    }
}
