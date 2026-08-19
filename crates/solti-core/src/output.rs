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
//! per-task lazy byte-bounded broadcast ring
//! within one aggregate payload budget
//!    │
//!    ▼
//! OutputSubscription
//! ```
//!
//! Output is live-only and best-effort.
//! It is not stored in task history.
//! A ring allocates storage only for events published while subscribers exist.
//! Channel creation does not allocate the configured event capacity.
//! Oversized chunks are exact prefixes with `truncated = true`.
//! Slow subscribers receive [`OutputEvent::Lagged`] with skipped event and
//! retained-payload byte counts.
//! A task continues without a live-output channel when the aggregate payload
//! budget cannot admit a new ring. The budget covers core-owned ring payload
//! and one internal post-lag event per subscription. It excludes events already
//! yielded to callers and copies queued for an external output sink.
//! External output callback copies use a separate hard count bound. Its default
//! is 2048 accepted events, including the active callback.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::SystemTime;

use bytes::Bytes;
use parking_lot::RwLock;
use solti_model::{OutputChunk, OutputEvent, TaskId, Uid};
use solti_runner::{OutputChunkRef, OutputPublisher, OutputSink};
use tokio::sync::{broadcast, watch};
use tokio_stream::Stream;
use tokio_stream::wrappers::WatchStream;

use crate::ConfigError;
use crate::persistence::{
    OutputEventDispatcher, PersistenceConfig, TaskOutputEvent, TaskOutputSinkHandle,
    TaskOutputSinkStatus,
};

/// Live output settings.
///
/// Event count and retained chunk bytes are bounded independently.
/// The broadcast ring uses the stricter of both limits.
/// Ring storage grows with retained events instead of allocating the configured
/// event capacity when a task channel is created.
/// An aggregate byte budget bounds core-owned ring and subscription-pending
/// payload. Caller-owned yielded events and external sink copies are separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    capacity: NonZeroUsize,
    byte_budget: NonZeroUsize,
    max_chunk_bytes: NonZeroUsize,
    aggregate_byte_budget: NonZeroUsize,
}

impl OutputConfig {
    /// Default per-task event capacity.
    pub const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();
    /// Default maximum bytes in one output chunk.
    pub const DEFAULT_MAX_CHUNK_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
    /// Default per-task retained chunk payload budget.
    pub const DEFAULT_BYTE_BUDGET: NonZeroUsize = NonZeroUsize::new(16 * 1024 * 1024).unwrap();
    /// Default aggregate live-output payload budget.
    pub const DEFAULT_AGGREGATE_BYTE_BUDGET: NonZeroUsize =
        NonZeroUsize::new(256 * 1024 * 1024).unwrap();

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
            aggregate_byte_budget: Self::DEFAULT_AGGREGATE_BYTE_BUDGET,
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

    /// Returns the aggregate core-owned live-output payload budget.
    ///
    /// Each ring reserves its maximum retained payload while the ring, one of
    /// its runner sinks, or one of its subscriptions remains alive. Each
    /// subscription separately reserves one maximum-size pending event.
    pub const fn aggregate_byte_budget(self) -> NonZeroUsize {
        self.aggregate_byte_budget
    }

    /// Returns the broadcast ring capacity after applying both limits.
    ///
    /// The ring preserves a power-of-two capacity. This method rounds down so
    /// the retained event count cannot exceed either configured limit.
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

    /// Sets the aggregate core-owned live-output payload budget.
    ///
    /// When a new ring cannot reserve its maximum payload, the task continues
    /// without a live-output channel. When a subscription cannot reserve its
    /// pending-event allowance, subscription returns `None`. Existing owners
    /// keep their reservations.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `aggregate_byte_budget` is zero.
    pub const fn try_with_aggregate_byte_budget(
        mut self,
        aggregate_byte_budget: usize,
    ) -> Result<Self, ConfigError> {
        let Some(aggregate_byte_budget) = NonZeroUsize::new(aggregate_byte_budget) else {
            return Err(ConfigError::Zero {
                field: "output_aggregate_byte_budget",
            });
        };
        self.aggregate_byte_budget = aggregate_byte_budget;
        Ok(self)
    }

    fn broadcaster_payload_reservation(self) -> usize {
        self.effective_capacity()
            .get()
            .checked_mul(self.max_chunk_bytes.get())
            .expect("validated output limits must have a representable ring reservation")
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
/// A subscription reserves one maximum-size chunk from the aggregate payload
/// budget while it can hold an internal event behind a lag notification.
///
/// The stream implements [`tokio_stream::Stream`].
/// Its item type is [`OutputEvent`].
pub struct OutputSubscription {
    receiver: LazyBroadcastReceiver,
    wake: WatchStream<u64>,
    next_bytes: u64,
    pending_lag: u64,
    pending_event: Option<OutputEvent>,
    // Payload owners are declared last so every retained payload field drops
    // before its aggregate reservations are released.
    _ring_payload_lease: Arc<OutputPayloadLease>,
    _pending_payload_lease: Arc<OutputPayloadLease>,
}

impl OutputSubscription {
    fn new(
        receiver: LazyBroadcastReceiver,
        wake: watch::Receiver<u64>,
        ring_payload_lease: Arc<OutputPayloadLease>,
        pending_payload_lease: Arc<OutputPayloadLease>,
        next_bytes: u64,
    ) -> Self {
        Self {
            receiver,
            wake: WatchStream::new(wake),
            next_bytes,
            pending_lag: 0,
            pending_event: None,
            _ring_payload_lease: ring_payload_lease,
            _pending_payload_lease: pending_payload_lease,
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
            match this.receiver.try_recv() {
                Ok(output) => {
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
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    this.pending_lag = this.pending_lag.saturating_add(skipped);
                }
                Err(broadcast::error::TryRecvError::Closed) if this.pending_lag != 0 => {
                    let skipped = std::mem::take(&mut this.pending_lag);
                    let skipped_bytes = this.receiver.total_bytes().saturating_sub(this.next_bytes);
                    return Poll::Ready(Some(OutputEvent::Lagged {
                        skipped,
                        skipped_bytes,
                    }));
                }
                Err(broadcast::error::TryRecvError::Closed) => return Poll::Ready(None),
                Err(broadcast::error::TryRecvError::Empty) => {
                    match Pin::new(&mut this.wake).poll_next(cx) {
                        Poll::Ready(Some(_)) | Poll::Ready(None) => continue,
                        Poll::Pending => return Poll::Pending,
                    }
                }
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
    shared: Arc<LazyBroadcastShared>,
    wake: watch::Sender<u64>,
    capacity: usize,
    ring_payload_lease: Arc<OutputPayloadLease>,
    pending_payload_reservation: usize,
}

struct LazyBroadcastShared {
    state: parking_lot::Mutex<LazyBroadcastState>,
}

struct LazyBroadcastState {
    events: VecDeque<LazyBroadcastSlot>,
    next_sequence: u128,
    receiver_count: usize,
    total_bytes: u64,
    revision: u64,
    closed: bool,
}

struct LazyBroadcastSlot {
    sequence: u128,
    output: BroadcastOutput,
    remaining: usize,
}

struct LazyBroadcastReceiver {
    shared: Arc<LazyBroadcastShared>,
    next_sequence: u128,
}

impl OutputBroadcaster {
    fn new(
        capacity: usize,
        ring_payload_lease: Arc<OutputPayloadLease>,
        pending_payload_reservation: usize,
    ) -> Self {
        let (wake, _initial_receiver) = watch::channel(0);
        Self {
            shared: Arc::new(LazyBroadcastShared {
                state: parking_lot::Mutex::new(LazyBroadcastState {
                    events: VecDeque::new(),
                    next_sequence: 0,
                    receiver_count: 0,
                    total_bytes: 0,
                    revision: 0,
                    closed: false,
                }),
            }),
            wake,
            capacity,
            ring_payload_lease,
            pending_payload_reservation,
        }
    }

    fn subscribe(&self) -> Option<OutputSubscription> {
        let pending_payload_lease = self
            .ring_payload_lease
            .budget
            .try_reserve(self.pending_payload_reservation)?;
        let wake = self.wake.subscribe();
        let (receiver, next_bytes) = self.register_receiver()?;
        Some(OutputSubscription::new(
            receiver,
            wake,
            Arc::clone(&self.ring_payload_lease),
            pending_payload_lease,
            next_bytes,
        ))
    }

    fn send(&self, event: OutputEvent) {
        let mut state = self.shared.state.lock();
        let bytes_before = state.total_bytes;
        let bytes_after = bytes_before.saturating_add(output_payload_bytes(&event));
        state.total_bytes = bytes_after;
        if state.receiver_count == 0 {
            return;
        }

        if state.events.len() == self.capacity {
            state.events.pop_front();
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let remaining = state.receiver_count;
        state.events.push_back(LazyBroadcastSlot {
            sequence,
            output: BroadcastOutput {
                event,
                bytes_before,
                bytes_after,
            },
            remaining,
        });
        state.revision = state.revision.wrapping_add(1);
        let revision = state.revision;
        drop(state);
        self.wake.send_replace(revision);
    }

    fn register_receiver(&self) -> Option<(LazyBroadcastReceiver, u64)> {
        let mut state = self.shared.state.lock();
        if state.closed {
            return None;
        }
        state.receiver_count = state.receiver_count.checked_add(1)?;
        Some((
            LazyBroadcastReceiver {
                shared: Arc::clone(&self.shared),
                next_sequence: state.next_sequence,
            },
            state.total_bytes,
        ))
    }

    #[cfg(test)]
    fn subscribe_raw(&self) -> Option<RawOutputReceiver> {
        let pending_payload_lease = self
            .ring_payload_lease
            .budget
            .try_reserve(self.pending_payload_reservation)?;
        let (receiver, _) = self.register_receiver()?;
        Some(RawOutputReceiver {
            receiver,
            _ring_payload_lease: Arc::clone(&self.ring_payload_lease),
            _pending_payload_lease: pending_payload_lease,
        })
    }

    #[cfg(test)]
    fn retained_events_and_allocation(&self) -> (usize, usize) {
        let state = self.shared.state.lock();
        (state.events.len(), state.events.capacity())
    }
}

impl Drop for OutputBroadcaster {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.closed = true;
        state.revision = state.revision.wrapping_add(1);
        let revision = state.revision;
        drop(state);
        self.wake.send_replace(revision);
    }
}

impl LazyBroadcastReceiver {
    fn try_recv(&mut self) -> Result<BroadcastOutput, broadcast::error::TryRecvError> {
        let mut state = self.shared.state.lock();
        let Some(front) = state.events.front() else {
            return if state.closed {
                Err(broadcast::error::TryRecvError::Closed)
            } else {
                Err(broadcast::error::TryRecvError::Empty)
            };
        };

        if self.next_sequence < front.sequence {
            let skipped = front.sequence - self.next_sequence;
            self.next_sequence = front.sequence;
            return Err(broadcast::error::TryRecvError::Lagged(
                u64::try_from(skipped).unwrap_or(u64::MAX),
            ));
        }

        let offset = self.next_sequence - front.sequence;
        let Ok(offset) = usize::try_from(offset) else {
            return if state.closed {
                Err(broadcast::error::TryRecvError::Closed)
            } else {
                Err(broadcast::error::TryRecvError::Empty)
            };
        };
        let Some(slot) = state.events.get_mut(offset) else {
            return if state.closed {
                Err(broadcast::error::TryRecvError::Closed)
            } else {
                Err(broadcast::error::TryRecvError::Empty)
            };
        };

        let output = slot.output.clone();
        slot.remaining = slot
            .remaining
            .checked_sub(1)
            .expect("a live output receiver must own each unread retained event");
        self.next_sequence = self.next_sequence.saturating_add(1);
        prune_consumed_events(&mut state.events);
        Ok(output)
    }

    fn total_bytes(&self) -> u64 {
        self.shared.state.lock().total_bytes
    }
}

impl Drop for LazyBroadcastReceiver {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock();
        state.receiver_count = state
            .receiver_count
            .checked_sub(1)
            .expect("a live output receiver must remain registered until drop");
        for slot in &mut state.events {
            if slot.sequence >= self.next_sequence {
                slot.remaining = slot
                    .remaining
                    .checked_sub(1)
                    .expect("a live output receiver must own each unread retained event");
            }
        }
        if state.receiver_count == 0 {
            state.events = VecDeque::new();
        } else {
            prune_consumed_events(&mut state.events);
        }
    }
}

fn prune_consumed_events(events: &mut VecDeque<LazyBroadcastSlot>) {
    while events.front().is_some_and(|slot| slot.remaining == 0) {
        events.pop_front();
    }
}

fn output_payload_bytes(event: &OutputEvent) -> u64 {
    match event {
        OutputEvent::Chunk(chunk) => u64::try_from(chunk.line.len()).unwrap_or(u64::MAX),
        _ => 0,
    }
}

#[cfg(test)]
pub(crate) struct RawOutputReceiver {
    receiver: LazyBroadcastReceiver,
    _ring_payload_lease: Arc<OutputPayloadLease>,
    _pending_payload_lease: Arc<OutputPayloadLease>,
}

#[cfg(test)]
impl RawOutputReceiver {
    pub(crate) fn try_recv(&mut self) -> Result<OutputEvent, broadcast::error::TryRecvError> {
        self.receiver.try_recv().map(|output| output.event)
    }
}

struct OutputPayloadBudget {
    limit: usize,
    reserved: AtomicUsize,
}

impl OutputPayloadBudget {
    fn new(limit: NonZeroUsize) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.get(),
            reserved: AtomicUsize::new(0),
        })
    }

    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<Arc<OutputPayloadLease>> {
        let reserved = self
            .reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |reserved| {
                (bytes <= self.limit.saturating_sub(reserved)).then_some(reserved + bytes)
            })
            .ok()?;
        debug_assert!(reserved <= self.limit.saturating_sub(bytes));
        Some(Arc::new(OutputPayloadLease {
            budget: Arc::clone(self),
            bytes,
        }))
    }
}

struct OutputPayloadLease {
    budget: Arc<OutputPayloadBudget>,
    bytes: usize,
}

impl Drop for OutputPayloadLease {
    fn drop(&mut self) {
        self.budget
            .reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |reserved| {
                reserved.checked_sub(self.bytes)
            })
            .expect("live output payload reservation must not underflow");
    }
}

/// Core-owned output channel registry.
pub(crate) struct OutputHub {
    channels: RwLock<HashMap<TaskId, OutputChannel>>,
    capacity: usize,
    max_chunk_bytes: usize,
    broadcaster_payload_reservation: usize,
    payload_budget: Arc<OutputPayloadBudget>,
    event_sink: Option<Arc<OutputEventDispatcher>>,
}

struct OutputChannel {
    task_uid: Uid,
    broadcaster: Arc<OutputBroadcaster>,
}

impl OutputHub {
    #[cfg(test)]
    pub(crate) fn new(config: OutputConfig) -> Self {
        Self::try_with_sink(config, None, PersistenceConfig::default())
            .expect("an output hub without a persistence sink cannot fail to start")
    }

    #[cfg(test)]
    pub(crate) fn with_sink(
        config: OutputConfig,
        event_sink: Option<TaskOutputSinkHandle>,
    ) -> Self {
        Self::try_with_sink(config, event_sink, PersistenceConfig::default())
            .expect("the test output persistence worker must start")
    }

    pub(crate) fn try_with_sink(
        config: OutputConfig,
        event_sink: Option<TaskOutputSinkHandle>,
        persistence_config: PersistenceConfig,
    ) -> io::Result<Self> {
        let event_sink = event_sink
            .map(|sink| {
                OutputEventDispatcher::start(sink, persistence_config.output_queue_capacity())
                    .map(Arc::new)
            })
            .transpose()?;
        Ok(Self {
            channels: RwLock::new(HashMap::new()),
            capacity: config.effective_capacity().get(),
            max_chunk_bytes: config.max_chunk_bytes().get(),
            broadcaster_payload_reservation: config.broadcaster_payload_reservation(),
            payload_budget: OutputPayloadBudget::new(config.aggregate_byte_budget()),
            event_sink,
        })
    }

    /// Ensures that a task channel exists.
    ///
    /// Returns `true` when this call creates it.
    pub(crate) fn ensure_channel_if_absent(&self, task_id: TaskId, task_uid: Uid) -> bool {
        let mut channels = self.channels.write();
        match channels.entry(task_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Some(payload_lease) = self
                    .payload_budget
                    .try_reserve(self.broadcaster_payload_reservation)
                else {
                    tracing::warn!(
                        event = "output.channel_unavailable",
                        task_uid = %task_uid,
                        reserved_bytes = self.payload_budget.reserved.load(Ordering::Acquire),
                        requested_bytes = self.broadcaster_payload_reservation,
                        aggregate_byte_budget = self.payload_budget.limit,
                        "live output aggregate payload budget is saturated"
                    );
                    return false;
                };
                entry.insert(OutputChannel {
                    task_uid,
                    broadcaster: Arc::new(OutputBroadcaster::new(
                        self.capacity,
                        payload_lease,
                        self.max_chunk_bytes,
                    )),
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
        let channels = self.channels.read();
        let channel = channels.get(task_id)?;
        channel.broadcaster.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn subscribe_raw(&self, task_id: &TaskId) -> Option<RawOutputReceiver> {
        let channels = self.channels.read();
        let channel = channels.get(task_id)?;
        channel.broadcaster.subscribe_raw()
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
        if let Some(event_sink) = &self.event_sink {
            let _ = event_sink.try_dispatch(TaskOutputEvent::new(
                task_id.clone(),
                task_uid.clone(),
                event,
            ));
        }
    }

    pub(crate) fn evict(&self, task_id: &TaskId) {
        self.channels.write().remove(task_id);
    }

    /// Evicts a channel only when it still belongs to the exact task identity.
    pub(crate) fn evict_if_uid(&self, task_id: &TaskId, task_uid: &Uid) -> bool {
        let mut channels = self.channels.write();
        let matches = channels
            .get(task_id)
            .is_some_and(|channel| &channel.task_uid == task_uid);
        matches && channels.remove(task_id).is_some()
    }

    #[cfg(test)]
    pub(crate) fn active_channels(&self) -> usize {
        self.channels.read().len()
    }

    #[cfg(test)]
    pub(crate) fn reserved_payload_bytes(&self) -> usize {
        self.payload_budget.reserved.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn retained_events_and_allocation(&self, task_id: &TaskId) -> Option<(usize, usize)> {
        self.channels
            .read()
            .get(task_id)
            .map(|channel| channel.broadcaster.retained_events_and_allocation())
    }

    pub(crate) fn persistence_status(&self) -> Option<TaskOutputSinkStatus> {
        self.event_sink.as_ref().map(|sink| sink.status())
    }

    pub(crate) async fn shutdown_persistence(&self) {
        if let Some(event_sink) = &self.event_sink {
            event_sink.shutdown().await;
        }
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
                if let Some(event_sink) = &event_sink {
                    let _ = event_sink.try_dispatch(TaskOutputEvent::new(
                        task_id.clone(),
                        task_uid.clone(),
                        event,
                    ));
                }
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use bytes::Bytes;
    use solti_model::{OutputEvent, TaskId, Uid};
    use solti_runner::OutputPublisher;
    use tokio_stream::StreamExt;

    use super::{ConfigError, OutputConfig, OutputHub};
    use crate::{PersistenceConfig, TaskOutputEvent, TaskOutputSink, TaskOutputSinkHandle};

    #[derive(Default)]
    struct RecordingOutputSink {
        events: Mutex<Vec<TaskOutputEvent>>,
    }

    impl TaskOutputSink for RecordingOutputSink {
        fn on_event(&self, event: &TaskOutputEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    struct BlockingFirstOutputSink {
        first: AtomicBool,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl TaskOutputSink for BlockingFirstOutputSink {
        fn on_event(&self, _event: &TaskOutputEvent) {
            if self.first.swap(false, Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
        }
    }

    #[test]
    fn config_preserves_default_and_rejects_zero() {
        let default = OutputConfig::default();
        assert_eq!(default.capacity(), OutputConfig::DEFAULT_CAPACITY);
        assert_eq!(default.effective_capacity(), OutputConfig::DEFAULT_CAPACITY);
        assert_eq!(default.byte_budget(), OutputConfig::DEFAULT_BYTE_BUDGET);
        assert_eq!(
            default.aggregate_byte_budget(),
            OutputConfig::DEFAULT_AGGREGATE_BYTE_BUDGET
        );
        assert_eq!(
            default.max_chunk_bytes(),
            OutputConfig::DEFAULT_MAX_CHUNK_BYTES
        );
        assert_eq!(OutputConfig::DEFAULT_CAPACITY.get(), 256);
        assert_eq!(OutputConfig::DEFAULT_BYTE_BUDGET.get(), 16 * 1024 * 1024);
        assert_eq!(OutputConfig::DEFAULT_MAX_CHUNK_BYTES.get(), 64 * 1024);
        assert_eq!(
            OutputConfig::DEFAULT_AGGREGATE_BYTE_BUDGET.get(),
            256 * 1024 * 1024
        );
        assert_eq!(
            OutputConfig::try_new(0).unwrap_err(),
            ConfigError::Zero {
                field: "output_capacity"
            }
        );
        assert_eq!(
            OutputConfig::default()
                .try_with_aggregate_byte_budget(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "output_aggregate_byte_budget"
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

    #[tokio::test]
    async fn maximum_config_constructs_an_empty_lazy_ring_and_keeps_future_only_delivery() {
        let config = OutputConfig::try_new(usize::MAX)
            .unwrap()
            .try_with_byte_limits(usize::MAX, 1)
            .unwrap()
            .try_with_aggregate_byte_budget(usize::MAX)
            .unwrap();
        let effective_capacity = config.effective_capacity().get();
        let hub = OutputHub::new(config);
        let task_id = TaskId::new("maximum-lazy-output").unwrap();

        assert!(hub.ensure_channel_if_absent(
            task_id.clone(),
            Uid::new("maximum-lazy-output-uid").unwrap()
        ));
        assert_eq!(hub.retained_events_and_allocation(&task_id), Some((0, 0)));

        let sink = hub.sink_for(&task_id, 1, 1).expect("output sink");
        sink.stdout_line(Bytes::from_static(b"x"));
        assert_eq!(
            hub.retained_events_and_allocation(&task_id),
            Some((0, 0)),
            "publishing without a subscriber must not allocate or retain a ring entry"
        );

        let mut output = hub.subscribe(&task_id).expect("output subscription");
        sink.stdout_line(Bytes::from_static(b"y"));
        let (retained, allocated) = hub
            .retained_events_and_allocation(&task_id)
            .expect("live channel");
        assert_eq!(retained, 1);
        assert!(
            allocated < effective_capacity,
            "one retained event must not allocate the configured maximum ring"
        );
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"y"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), output.next())
                .await
                .is_err(),
            "a subscriber must not receive output published before subscription"
        );

        drop(output);
        assert_eq!(
            hub.retained_events_and_allocation(&task_id),
            Some((0, 0)),
            "dropping the last subscriber must release lazy ring storage"
        );
    }

    #[test]
    fn producer_cannot_create_a_task_channel() {
        let hub = OutputHub::new(OutputConfig::default());
        let task_id = TaskId::new("missing").unwrap();

        assert!(hub.sink_for(&task_id, 1, 1).is_none());
        assert_eq!(hub.active_channels(), 0);
    }

    #[test]
    fn aggregate_payload_budget_admits_exact_reservations() {
        let config = OutputConfig::try_new(1)
            .unwrap()
            .try_with_byte_limits(4, 4)
            .unwrap()
            .try_with_aggregate_byte_budget(8)
            .unwrap();
        let hub = OutputHub::new(config);
        let first = TaskId::new("aggregate-first").unwrap();
        let second = TaskId::new("aggregate-second").unwrap();
        let saturated = TaskId::new("aggregate-saturated").unwrap();

        assert!(hub.ensure_channel_if_absent(first.clone(), Uid::new("first-uid").unwrap()));
        assert_eq!(hub.reserved_payload_bytes(), 4);
        assert!(hub.ensure_channel_if_absent(second.clone(), Uid::new("second-uid").unwrap()));
        assert_eq!(hub.reserved_payload_bytes(), 8);
        assert!(
            !hub.ensure_channel_if_absent(saturated.clone(), Uid::new("saturated-uid").unwrap())
        );
        assert!(hub.sink_for(&saturated, 1, 1).is_none());
        assert_eq!(hub.active_channels(), 2);
        assert_eq!(hub.reserved_payload_bytes(), 8);

        hub.evict(&first);
        assert_eq!(hub.reserved_payload_bytes(), 4);
        assert!(
            hub.ensure_channel_if_absent(saturated.clone(), Uid::new("saturated-uid").unwrap())
        );
        assert!(hub.sink_for(&saturated, 1, 1).is_some());
        assert_eq!(hub.reserved_payload_bytes(), 8);
    }

    #[test]
    fn aggregate_payload_budget_rejects_a_ring_one_byte_past_the_boundary() {
        let config = OutputConfig::try_new(1)
            .unwrap()
            .try_with_byte_limits(4, 4)
            .unwrap()
            .try_with_aggregate_byte_budget(3)
            .unwrap();
        let hub = OutputHub::new(config);
        let task_id = TaskId::new("aggregate-boundary").unwrap();

        assert!(!hub.ensure_channel_if_absent(
            task_id.clone(),
            Uid::new("aggregate-boundary-uid").unwrap()
        ));
        assert!(hub.sink_for(&task_id, 1, 1).is_none());
        assert_eq!(hub.active_channels(), 0);
        assert_eq!(hub.reserved_payload_bytes(), 0);
    }

    #[test]
    fn stale_sink_and_subscription_hold_the_aggregate_payload_lease() {
        let config = OutputConfig::try_new(1)
            .unwrap()
            .try_with_byte_limits(4, 4)
            .unwrap()
            .try_with_aggregate_byte_budget(8)
            .unwrap();
        let hub = OutputHub::new(config);
        let task_id = TaskId::new("aggregate-recreate").unwrap();
        let old_uid = Uid::new("aggregate-old-uid").unwrap();
        let new_uid = Uid::new("aggregate-new-uid").unwrap();

        assert!(hub.ensure_channel_if_absent(task_id.clone(), old_uid));
        let stale_sink = hub.sink_for(&task_id, 1, 1).expect("old sink");
        let stale_subscription = hub.subscribe(&task_id).expect("old subscription");
        hub.evict(&task_id);

        assert!(!hub.ensure_channel_if_absent(task_id.clone(), new_uid.clone()));
        assert_eq!(hub.reserved_payload_bytes(), 8);
        drop(stale_sink);
        assert!(!hub.ensure_channel_if_absent(task_id.clone(), new_uid.clone()));
        assert_eq!(hub.reserved_payload_bytes(), 8);
        drop(stale_subscription);

        assert!(hub.ensure_channel_if_absent(task_id.clone(), new_uid));
        assert_eq!(hub.reserved_payload_bytes(), 4);
    }

    #[tokio::test]
    async fn lagged_subscriptions_have_independent_pending_payload_leases() {
        let config = OutputConfig::try_new(1)
            .unwrap()
            .try_with_byte_limits(4, 4)
            .unwrap()
            .try_with_aggregate_byte_budget(12)
            .unwrap();
        let hub = OutputHub::new(config);
        let task_id = TaskId::new("aggregate-lagged-subscribers").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut first = hub.subscribe(&task_id).expect("first subscription");
        let mut second = hub.subscribe(&task_id).expect("second subscription");
        assert_eq!(hub.reserved_payload_bytes(), 12);
        assert!(
            hub.subscribe(&task_id).is_none(),
            "a third pending-event lease must exceed the aggregate budget"
        );
        let sink = hub.sink_for(&task_id, 1, 1).expect("output sink");

        sink.stdout_line(Bytes::from_static(b"aaaa"));
        sink.stdout_line(Bytes::from_static(b"bbbb"));
        assert!(matches!(
            first.next().await,
            Some(OutputEvent::Lagged {
                skipped: 1,
                skipped_bytes: 4
            })
        ));

        sink.stdout_line(Bytes::from_static(b"cccc"));
        sink.stdout_line(Bytes::from_static(b"dddd"));
        assert!(matches!(
            second.next().await,
            Some(OutputEvent::Lagged {
                skipped: 3,
                skipped_bytes: 12
            })
        ));
        assert_eq!(hub.reserved_payload_bytes(), 12);
        assert!(matches!(
            first.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"bbbb"
        ));
        assert!(matches!(
            second.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"dddd"
        ));

        drop(first);
        assert_eq!(hub.reserved_payload_bytes(), 8);
        let third = hub
            .subscribe(&task_id)
            .expect("released pending-event lease must admit a new subscription");
        assert_eq!(hub.reserved_payload_bytes(), 12);
        drop(third);
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

    #[tokio::test]
    async fn external_sink_receives_run_markers_and_first_chunk() {
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
        hub.shutdown_persistence().await;

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

        hub.shutdown_persistence().await;

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

    #[tokio::test]
    async fn stale_output_keeps_the_original_task_uid() {
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
        hub.shutdown_persistence().await;

        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].task_uid(), &old_uid);
        assert_eq!(events[1].task_uid(), &new_uid);
    }

    #[tokio::test]
    async fn callback_overload_drops_only_the_callback_copy() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let persistence: TaskOutputSinkHandle = Arc::new(BlockingFirstOutputSink {
            first: AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let persistence_config = PersistenceConfig::new()
            .try_with_output_queue_capacity(1)
            .unwrap();
        let hub = OutputHub::try_with_sink(
            OutputConfig::try_new(4).unwrap(),
            Some(persistence),
            persistence_config,
        )
        .unwrap();
        let task_id = TaskId::new("output-callback-overload").unwrap();
        hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("attempt sink");

        sink.stdout_line(Bytes::from_static(b"first"));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first output callback must become active");
        sink.stdout_line(Bytes::from_static(b"second"));

        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"first"
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"second"
        ));
        let status = hub.persistence_status().unwrap();
        assert_eq!(status.queued(), 1);
        assert_eq!(status.capacity(), 1);
        assert_eq!(status.delivered(), 0);
        assert_eq!(status.failed(), 0);
        assert_eq!(status.dropped(), 1);
        assert!(status.accepting());
        assert!(status.healthy());

        release_tx.send(()).unwrap();
        hub.shutdown_persistence().await;
        let status = hub.persistence_status().unwrap();
        assert_eq!(status.queued(), 0);
        assert_eq!(status.delivered(), 1);
        assert_eq!(status.dropped(), 1);
        assert!(!status.accepting());
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
