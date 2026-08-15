//! # Task state
//!
//! [`TaskState`] is the in-memory resource store of `solti-core`.
//!
//! ## Flow
//!
//! ```text
//! desired writes ────────────────┐
//! Taskvisor events ──────────────┤
//! direct completion outcomes ────┤
//!                                ▼
//!                           TaskState
//!                                ├──► Task reads
//!                                ├──► TaskRun history
//!                                ├──► list snapshots
//!                                └──► Task watches
//! ```
//!
//! Normal writes belong to [`SupervisorApi`](crate::SupervisorApi).
//! Public `TaskState` methods provide shared read access.
//!
//! Each store has distinct Task and TaskRun resource-version epochs.
//! Task list continuations and watches share retained Task changes.
//! TaskRun continuations use a separate reversible mutation journal.
//! Retained runs and run-journal deltas share immutable [`Arc<TaskRun>`] snapshots.
//! Run mutations use copy-on-write. Queries clone handles under the state lock
//! and clone model values only when admitting them to a page.
//! Each continuation can resume only while its journal position remains available.
//! Both journals are limited by count and serialized bytes.
//! Task watch admission is limited by concurrent subscription count and the
//! aggregate compact Task JSON retained by initial and replay buffers.
//! The live watch ring is smaller than a multi-entry change journal, leaving
//! count headroom for recovery after a slow subscriber lags. The independent
//! byte budget can still compact the required changes.
//! An oversized Task change remains live on the watch stream but is not retained.
//! An oversized TaskRun batch updates current run state but is not retained.
//! [`StateConfig::max_retained_tasks`] limits retained task resources.
//! [`StateConfig::max_retained_task_manifest_bytes`] limits their aggregate
//! caller-owned manifest bytes.
//! Admission never evicts a task to admit another task.
//! Task query pages keep complete-item prefixes within a 4 MiB serialized JSON budget.
//! TaskRun query pages use the same complete-item budget.
//! An oversized first item is returned alone for native transport measurement.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::Future,
    io::{self, Write},
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::SystemTime,
};

use parking_lot::{Condvar, Mutex, RwLock, RwLockWriteGuard};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::{
    Stream,
    wrappers::{BroadcastStream, errors::BroadcastStreamRecvError},
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use solti_model::{
    DesiredChange, Slot, Task, TaskContinuation, TaskFilter, TaskId, TaskManifest, TaskPage,
    TaskPhase, TaskQuery, TaskRun, TaskRunContinuation, TaskRunPage, TaskRunQuery, TaskWatchEvent,
    Uid, WorkloadTypeMeta, WritePreconditions,
};

use crate::persistence::{
    MAX_STATE_EVENTS_PER_COMMIT, PersistenceConfig, StateDispatchEvent, StateEventDispatcher,
    StateQueuePermit, TaskStateEvent, TaskStateSinkHandle, TaskStateSinkStatus,
};
use crate::{StateConfig, WriteConflict, WritePreconditionViolation, error::CoreError};

/// Shared in-memory task state.
///
/// `TaskState` is usually obtained from [`SupervisorApi::state`](crate::SupervisorApi::state).
/// Its clones share one store.
/// The supervisor owns normal writes.
///
/// ## Example
///
/// ```
/// use solti_core::TaskState;
/// use solti_model::TaskQuery;
///
/// let state = TaskState::new();
/// assert!(state.list_all().is_empty());
/// assert!(state.query(&TaskQuery::new()).unwrap().items.is_empty());
/// ```
#[derive(Clone)]
pub struct TaskState {
    inner: Arc<RwLock<TaskStateInner>>,
    watch_stop: CancellationToken,
    watch_admission: Arc<WatchAdmission>,
    event_publisher: Arc<StateEventPublisher>,
}

struct StateEventPublisher {
    dispatcher: Option<StateEventDispatcher>,
    inner: Mutex<StateEventPublisherInner>,
}

#[derive(Default)]
struct StateEventPublisherInner {
    pending: VecDeque<Arc<PendingStateEventBatch>>,
    publishing: bool,
}

struct PendingStateEventBatch {
    inner: Mutex<PendingStateEventBatchInner>,
    ready: AtomicBool,
    published: AtomicBool,
    completion: Condvar,
}

struct PendingStateEventBatchInner {
    events: VecDeque<StateDispatchEvent>,
    permits: VecDeque<StateQueuePermit>,
}

impl PendingStateEventBatch {
    fn new(permits: Vec<StateQueuePermit>) -> Self {
        Self {
            inner: Mutex::new(PendingStateEventBatchInner {
                events: VecDeque::new(),
                permits: permits.into(),
            }),
            ready: AtomicBool::new(false),
            published: AtomicBool::new(false),
            completion: Condvar::new(),
        }
    }

    fn enqueue(&self, event: TaskStateEvent) {
        let mut inner = self.inner.lock();
        let permit = inner
            .permits
            .pop_front()
            .expect("a state mutation must reserve its maximum event count before the write lock");
        inner
            .events
            .push_back(StateDispatchEvent::new(event, permit));
    }

    fn release_unused_permits(&self) {
        self.inner.lock().permits.clear();
    }
}

impl StateEventPublisher {
    fn new(
        sink: Option<TaskStateSinkHandle>,
        config: PersistenceConfig,
    ) -> Result<Self, io::Error> {
        let dispatcher = sink
            .map(|sink| StateEventDispatcher::start(sink, config.state_queue_capacity()))
            .transpose()?;
        Ok(Self {
            dispatcher,
            inner: Mutex::new(StateEventPublisherInner::default()),
        })
    }

    fn reserve(&self, event_capacity: usize) -> Option<Vec<StateQueuePermit>> {
        let dispatcher = self.dispatcher.as_ref()?;
        let permits = dispatcher.reserve(event_capacity);
        (!permits.is_empty()).then_some(permits)
    }

    fn begin_batch(
        &self,
        permits: Option<Vec<StateQueuePermit>>,
    ) -> Option<Arc<PendingStateEventBatch>> {
        let permits = permits?;
        let batch = Arc::new(PendingStateEventBatch::new(permits));
        self.inner.lock().pending.push_back(Arc::clone(&batch));
        Some(batch)
    }

    fn mark_ready_and_publish(&self, batch: Arc<PendingStateEventBatch>) {
        batch.ready.store(true, Ordering::Release);
        self.publish_pending();
        let mut inner = batch.inner.lock();
        while !batch.published.load(Ordering::Acquire) {
            batch.completion.wait(&mut inner);
        }
    }

    fn publish_pending(&self) {
        let Some(dispatcher) = self.dispatcher.as_ref() else {
            return;
        };
        {
            let mut inner = self.inner.lock();
            if inner.publishing || inner.pending.is_empty() {
                return;
            }
            inner.publishing = true;
        }

        loop {
            let (batch, events) = {
                let mut inner = self.inner.lock();
                let Some(batch) = inner.pending.front().cloned() else {
                    inner.publishing = false;
                    return;
                };
                if !batch.ready.load(Ordering::Acquire) {
                    inner.publishing = false;
                    return;
                }
                let events = batch.inner.lock().events.drain(..).collect::<Vec<_>>();
                inner.pending.pop_front();
                (batch, events)
            };
            dispatcher.dispatch(events);
            let _inner = batch.inner.lock();
            batch.published.store(true, Ordering::Release);
            batch.completion.notify_all();
        }
    }

    async fn shutdown(&self) {
        if let Some(dispatcher) = self.dispatcher.as_ref() {
            dispatcher.shutdown().await;
        }
    }

    fn status(&self) -> Option<TaskStateSinkStatus> {
        self.dispatcher.as_ref().map(StateEventDispatcher::status)
    }
}

struct TaskStateWriteGuard<'a> {
    inner: Option<RwLockWriteGuard<'a, TaskStateInner>>,
    watch_admission: &'a WatchAdmission,
    watch_history_invalidated: bool,
    publisher: &'a StateEventPublisher,
    batch: Option<Arc<PendingStateEventBatch>>,
}

#[derive(Clone, Copy)]
enum StateMutationEventCapacity {
    None,
    TaskChange,
    TaskAndRunChange,
    AttemptTransition,
}

impl StateMutationEventCapacity {
    const fn get(self) -> usize {
        match self {
            Self::None => 0,
            Self::TaskChange => 1,
            Self::TaskAndRunChange => 2,
            Self::AttemptTransition => MAX_STATE_EVENTS_PER_COMMIT,
        }
    }
}

impl TaskStateWriteGuard<'_> {
    fn enqueue(&self, event: TaskStateEvent) {
        if let Some(batch) = self.batch.as_ref() {
            batch.enqueue(event);
        }
    }

    fn invalidate_watch_history(&mut self) {
        self.watch_history_invalidated = true;
    }
}

impl Deref for TaskStateWriteGuard<'_> {
    type Target = TaskStateInner;

    fn deref(&self) -> &Self::Target {
        self.inner
            .as_deref()
            .expect("TaskState write guard is present until drop")
    }
}

impl DerefMut for TaskStateWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
            .as_deref_mut()
            .expect("TaskState write guard is present until drop")
    }
}

impl Drop for TaskStateWriteGuard<'_> {
    fn drop(&mut self) {
        let batch = self.batch.take();
        let watch_history_invalidated = self.watch_history_invalidated;
        // Readiness must become visible only after the authoritative state lock is released.
        drop(self.inner.take());
        if watch_history_invalidated {
            self.watch_admission.notify_history_changed();
        }
        if let Some(batch) = batch {
            batch.release_unused_permits();
            self.publisher.mark_ready_and_publish(batch);
        }
    }
}

struct TaskStateInner {
    /// Tasks indexed by model task name.
    ///
    /// Stored snapshots are immutable and shared with watch history, persistence,
    /// and pagination. A state transition clones the resource only when it is
    /// actually mutated through [`Arc::make_mut`].
    tasks: HashMap<TaskId, Arc<Task>>,
    /// Task names in stable pagination and watch-snapshot order.
    ordered_tasks: BTreeSet<TaskId>,
    /// Task names indexed by slot.
    by_slot: HashMap<Slot, BTreeSet<TaskId>>,
    /// Immutable run snapshots indexed by task name.
    ///
    /// Pagination and the reversible journal share these allocations.
    runs: HashMap<TaskId, VecDeque<Arc<TaskRun>>>,
    /// Store identity embedded in each TaskRun collection version.
    run_resource_version_epoch: String,
    /// Latest committed TaskRun mutation-batch counter.
    run_resource_version: u64,
    /// Reversible TaskRun mutation batches retained for list snapshots.
    run_history: VecDeque<RawRunChangeBatch>,
    /// Serialized bytes retained in the TaskRun journal.
    run_history_bytes: usize,
    /// Maximum serialized TaskRun journal bytes.
    run_history_byte_budget: usize,
    /// Highest compacted TaskRun revision.
    run_compacted_through: u64,
    /// Maximum retained TaskRun mutation-batch count.
    run_history_capacity: usize,
    /// Taskvisor identity to exact resource generation.
    by_tv: HashMap<u64, RuntimeBinding>,
    /// Resource name to current Taskvisor binding.
    tv_of: HashMap<TaskId, RuntimeBinding>,
    /// Highest projected terminal attempt for each live binding.
    ///
    /// This survives visible run eviction.
    /// Duplicate terminal events therefore remain idempotent.
    finished_attempt_by_tv: HashMap<u64, u32>,
    /// Store identity embedded in each resource version.
    resource_version_epoch: Arc<str>,
    /// Latest committed resource-version counter.
    resource_version: u64,
    /// Changes retained for watches and list snapshots.
    watch_history: VecDeque<Arc<RawTaskChange>>,
    /// Serialized bytes retained in change history.
    watch_history_bytes: usize,
    /// Maximum serialized change-history bytes.
    watch_history_byte_budget: usize,
    /// Highest compacted revision.
    compacted_through: u64,
    /// Maximum retained change count.
    watch_history_capacity: usize,
    /// Live Task change broadcast.
    watch_tx: broadcast::Sender<Arc<RawTaskChange>>,
    /// Terminal transition time used by retention.
    terminal_since: HashMap<TaskId, SystemTime>,
    /// Per-task completed run cap.
    max_runs_per_task: usize,
    /// Maximum retained task count.
    max_retained_tasks: Option<std::num::NonZeroUsize>,
    /// Canonical compact JSON bytes for every retained caller-owned manifest.
    retained_task_manifest_bytes: usize,
    /// Retained caller-owned manifest bytes indexed by task name.
    retained_task_manifest_bytes_by_name: HashMap<TaskId, usize>,
    /// Maximum aggregate retained caller-owned manifest bytes.
    max_retained_task_manifest_bytes: Option<std::num::NonZeroUsize>,
}

struct RawTaskChange {
    /// Store epoch that committed this change.
    epoch: Arc<str>,
    revision: u64,
    previous: Option<Arc<Task>>,
    current: Option<Arc<Task>>,
    serialized_bytes: usize,
}

/// One atomically committed TaskRun revision and its reversible changes.
struct RawRunChangeBatch {
    /// Revision assigned to the complete mutation batch.
    revision: u64,
    /// Ordered changes committed by this mutation.
    changes: Vec<RawRunChange>,
    /// Compact JSON bytes charged to the run journal.
    serialized_bytes: usize,
}

/// One reversible TaskRun value change for an exact task identity.
struct RawRunChange {
    /// Task name owning the run.
    task: TaskId,
    /// Task UID owning the run at this revision.
    task_uid: Uid,
    /// Shared immutable value before the mutation.
    previous: Option<Arc<TaskRun>>,
    /// Shared immutable value after the mutation.
    current: Option<Arc<TaskRun>>,
}

#[derive(Default)]
struct SerializedSizeCounter(usize);

impl Write for SerializedSizeCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type WatchPredicate = Arc<dyn Fn(&Task) -> bool + Send + Sync>;

const WATCH_POLL_BUDGET: usize = 128;

fn task_watch_live_capacity(history_capacity: usize) -> usize {
    (history_capacity.next_power_of_two() / 2).max(1)
}

struct WatchAdmission {
    max_count: Option<usize>,
    max_bytes: Option<usize>,
    inner: Mutex<WatchAdmissionInner>,
}

struct WatchAdmissionInner {
    closed: bool,
    next_lease_id: u64,
    retained_bytes: usize,
    history_token: Arc<()>,
    leases: HashMap<u64, usize>,
    waiters: HashMap<u64, Waker>,
}

enum WatchAdmissionFailure {
    Closed,
    Rejected(CollectionError),
}

enum WatchReplayReservation {
    Reserved,
    Oversized,
    HistoryChanged,
    Closed,
}

struct WatchAdmissionLease {
    admission: Arc<WatchAdmission>,
    id: u64,
}

impl WatchAdmission {
    fn new(config: StateConfig) -> Arc<Self> {
        Arc::new(Self {
            max_count: config
                .max_concurrent_task_watches()
                .map(|limit| limit.get()),
            max_bytes: config
                .max_task_watch_initial_replay_bytes()
                .map(|limit| limit.get()),
            inner: Mutex::new(WatchAdmissionInner {
                closed: false,
                next_lease_id: 1,
                retained_bytes: 0,
                history_token: Arc::new(()),
                leases: HashMap::new(),
                waiters: HashMap::new(),
            }),
        })
    }

    fn precheck_count(&self) -> Result<(), WatchAdmissionFailure> {
        let inner = self.inner.lock();
        if inner.closed {
            return Err(WatchAdmissionFailure::Closed);
        }
        if let Some(limit) = self.max_count
            && inner.leases.len() >= limit
        {
            return Err(WatchAdmissionFailure::Rejected(
                CollectionError::ConcurrentTaskWatchLimitReached { limit },
            ));
        }
        Ok(())
    }

    fn try_admit(
        self: &Arc<Self>,
        requested_bytes: usize,
    ) -> Result<WatchAdmissionLease, WatchAdmissionFailure> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(WatchAdmissionFailure::Closed);
        }
        if let Some(limit) = self.max_count
            && inner.leases.len() >= limit
        {
            return Err(WatchAdmissionFailure::Rejected(
                CollectionError::ConcurrentTaskWatchLimitReached { limit },
            ));
        }
        if let Some(limit) = self.max_bytes
            && requested_bytes > limit - inner.retained_bytes
        {
            return Err(WatchAdmissionFailure::Rejected(
                CollectionError::TaskWatchInitialReplayByteLimitExceeded {
                    current: inner.retained_bytes,
                    requested: requested_bytes,
                    limit,
                },
            ));
        }

        let id = loop {
            let candidate = inner.next_lease_id;
            inner.next_lease_id = inner.next_lease_id.wrapping_add(1).max(1);
            if !inner.leases.contains_key(&candidate) {
                break candidate;
            }
        };
        let charged_bytes = self.max_bytes.map_or(0, |_| requested_bytes);
        inner.retained_bytes = inner
            .retained_bytes
            .checked_add(charged_bytes)
            .expect("watch admission bytes must remain within the configured limit");
        inner.leases.insert(id, charged_bytes);
        Ok(WatchAdmissionLease {
            admission: Arc::clone(self),
            id,
        })
    }

    fn poll_reserve_replay(
        &self,
        id: u64,
        requested_bytes: usize,
        history_token: &Arc<()>,
        cx: &Context<'_>,
    ) -> Poll<WatchReplayReservation> {
        let mut incoming_waker = Some(cx.waker().clone());
        let (result, removed_waker) = {
            let mut inner = self.inner.lock();
            if inner.closed || !inner.leases.contains_key(&id) {
                (Poll::Ready(WatchReplayReservation::Closed), None)
            } else if !Arc::ptr_eq(&inner.history_token, history_token) {
                let removed_waker = inner.waiters.remove(&id);
                (
                    Poll::Ready(WatchReplayReservation::HistoryChanged),
                    removed_waker,
                )
            } else if self.max_bytes.is_none() {
                (Poll::Ready(WatchReplayReservation::Reserved), None)
            } else {
                let limit = self
                    .max_bytes
                    .expect("the replay byte limit was checked as present");
                if requested_bytes > limit {
                    (Poll::Ready(WatchReplayReservation::Oversized), None)
                } else if requested_bytes <= limit - inner.retained_bytes {
                    inner.retained_bytes = inner
                        .retained_bytes
                        .checked_add(requested_bytes)
                        .expect("watch replay bytes must remain within the configured limit");
                    *inner
                        .leases
                        .get_mut(&id)
                        .expect("an admitted watch lease must remain registered") +=
                        requested_bytes;
                    let removed_waker = inner.waiters.remove(&id);
                    (Poll::Ready(WatchReplayReservation::Reserved), removed_waker)
                } else {
                    let removed_waker = match inner.waiters.entry(id) {
                        std::collections::hash_map::Entry::Occupied(mut waiter) => {
                            if waiter.get().will_wake(
                                incoming_waker
                                    .as_ref()
                                    .expect("the incoming replay waker must be available"),
                            ) {
                                None
                            } else {
                                Some(
                                    waiter.insert(
                                        incoming_waker
                                            .take()
                                            .expect("the incoming replay waker must be available"),
                                    ),
                                )
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(waiter) => {
                            waiter.insert(
                                incoming_waker
                                    .take()
                                    .expect("the incoming replay waker must be available"),
                            );
                            None
                        }
                    };
                    (Poll::Pending, removed_waker)
                }
            }
        };
        drop(removed_waker);
        drop(incoming_waker);
        result
    }

    fn history_token(&self) -> Arc<()> {
        Arc::clone(&self.inner.lock().history_token)
    }

    fn notify_history_changed(&self) {
        let waiters = {
            let mut inner = self.inner.lock();
            inner.history_token = Arc::new(());
            inner
                .waiters
                .drain()
                .map(|(_, waker)| waker)
                .collect::<Vec<_>>()
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    fn release_bytes(&self, id: u64, released_bytes: usize) {
        if self.max_bytes.is_none() || released_bytes == 0 {
            return;
        }
        let waiters = {
            let mut inner = self.inner.lock();
            let Some(lease_bytes) = inner.leases.get_mut(&id) else {
                return;
            };
            *lease_bytes = lease_bytes
                .checked_sub(released_bytes)
                .expect("watch lease byte accounting must not underflow");
            inner.retained_bytes = inner
                .retained_bytes
                .checked_sub(released_bytes)
                .expect("aggregate watch byte accounting must not underflow");
            inner
                .waiters
                .drain()
                .map(|(_, waker)| waker)
                .collect::<Vec<_>>()
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    fn release_lease(&self, id: u64) {
        let (removed_waiter, waiters) = {
            let mut inner = self.inner.lock();
            let Some(released_bytes) = inner.leases.remove(&id) else {
                return;
            };
            inner.retained_bytes = inner
                .retained_bytes
                .checked_sub(released_bytes)
                .expect("aggregate watch lease accounting must not underflow");
            let removed_waiter = inner.waiters.remove(&id);
            let waiters = inner
                .waiters
                .drain()
                .map(|(_, waker)| waker)
                .collect::<Vec<_>>();
            (removed_waiter, waiters)
        };
        drop(removed_waiter);
        for waiter in waiters {
            waiter.wake();
        }
    }

    fn close(&self) {
        let waiters = {
            let mut inner = self.inner.lock();
            if inner.closed {
                return;
            }
            inner.closed = true;
            inner.retained_bytes = 0;
            inner.leases.clear();
            inner
                .waiters
                .drain()
                .map(|(_, waker)| waker)
                .collect::<Vec<_>>()
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    #[cfg(test)]
    fn usage(&self) -> (usize, usize, usize) {
        let inner = self.inner.lock();
        (
            inner.leases.len(),
            inner.retained_bytes,
            inner.waiters.len(),
        )
    }
}

impl WatchAdmissionLease {
    fn is_active(&self) -> bool {
        self.admission.inner.lock().leases.contains_key(&self.id)
    }

    fn poll_reserve_replay(
        &self,
        requested_bytes: usize,
        history_token: &Arc<()>,
        cx: &Context<'_>,
    ) -> Poll<WatchReplayReservation> {
        self.admission
            .poll_reserve_replay(self.id, requested_bytes, history_token, cx)
    }

    fn history_token(&self) -> Arc<()> {
        self.admission.history_token()
    }

    fn release_bytes(&self, released_bytes: usize) {
        self.admission.release_bytes(self.id, released_bytes);
    }
}

impl Drop for WatchAdmissionLease {
    fn drop(&mut self) {
        self.admission.release_lease(self.id);
    }
}

/// Error from a task collection snapshot.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CollectionError {
    /// The state cannot admit another concurrent Task watch.
    #[error("concurrent Task watch limit reached: {limit}")]
    ConcurrentTaskWatchLimitReached {
        /// Configured concurrent Task watch limit.
        limit: usize,
    },
    /// A new Task watch would exceed the aggregate initial and replay byte budget.
    #[error(
        "Task watch initial and replay byte limit exceeded: current {current} bytes, requested {requested} bytes, limit {limit} bytes"
    )]
    TaskWatchInitialReplayByteLimitExceeded {
        /// Serialized bytes retained by admitted Task watches.
        current: usize,
        /// Serialized bytes required by the new Task watch.
        requested: usize,
        /// Configured aggregate byte limit.
        limit: usize,
    },
    /// The resource version is malformed or ahead of this store.
    #[error("invalid resourceVersion `{resource_version}`")]
    InvalidResourceVersion {
        /// Supplied resource version.
        resource_version: String,
    },
    /// The resource version is foreign or compacted.
    #[error("resourceVersion `{resource_version}` has expired")]
    ResourceVersionExpired {
        /// Expired resource version.
        resource_version: String,
    },
    /// Continuation filters differ from the first page.
    #[error("continuation filter does not match the query filter")]
    ContinuationFilterMismatch,
    /// The continuation task is absent from its filtered snapshot.
    #[error("continuation cursor `{name}` is not part of the retained snapshot")]
    ContinuationCursorNotFound {
        /// Missing task name.
        name: TaskId,
    },
    /// A TaskRun continuation belongs to another task name.
    #[error(
        "TaskRun continuation belongs to task `{continuation_task}`, not requested task `{task}`"
    )]
    TaskRunContinuationTaskMismatch {
        /// Requested task name.
        task: TaskId,
        /// Task name fixed by the continuation.
        continuation_task: TaskId,
    },
    /// The TaskRun continuation cursor is absent from its retained snapshot.
    #[error(
        "TaskRun continuation cursor `{task}` generation {generation} attempt {attempt} is not part of the retained snapshot"
    )]
    TaskRunContinuationCursorNotFound {
        /// Task name fixed by the continuation.
        task: TaskId,
        /// Missing run generation.
        generation: u64,
        /// Missing run attempt.
        attempt: u32,
    },
}

/// Stream of filtered task changes.
///
/// Items are [`TaskWatchEvent`] values wrapped in [`Result`].
/// The stream ends when its supervisor shuts down.
///
/// A compacted resume point produces [`CollectionError::ResourceVersionExpired`].
/// That error is terminal.
/// The stream ends after returning it.
///
/// Initial and exact-resume events retain an aggregate byte lease until each
/// event is yielded. Lag recovery waits for byte capacity without retaining
/// replay payload across a pending poll. One event larger than the complete
/// budget is transferred directly to the caller and is never buffered.
#[must_use = "streams do nothing unless polled"]
pub struct TaskWatchSubscription {
    inner: Arc<RwLock<TaskStateInner>>,
    receiver: BroadcastStream<Arc<RawTaskChange>>,
    initial: VecDeque<BufferedWatchEvent>,
    initial_revision: Option<u64>,
    replay: VecDeque<PreparedReplayChange>,
    recovery: Option<LagRecovery>,
    permit: Option<WatchAdmissionLease>,
    filter: TaskFilter,
    predicate: WatchPredicate,
    epoch: Arc<str>,
    last_revision: u64,
    stop: Pin<Box<dyn Future<Output = ()> + Send>>,
    terminal: bool,
}

#[derive(Clone, Copy)]
enum PreparedWatchEventKind {
    Added,
    Modified,
    Deleted,
}

struct WatchEventDescriptor {
    kind: PreparedWatchEventKind,
    task: Arc<Task>,
    resource_version: Option<String>,
    serialized_bytes: usize,
}

impl WatchEventDescriptor {
    fn materialize(self) -> TaskWatchEvent {
        let mut task = self.task.as_ref().clone();
        if let Some(resource_version) = self.resource_version {
            task.set_resource_version(resource_version)
                .expect("store resource version must be valid");
        }
        match self.kind {
            PreparedWatchEventKind::Added => TaskWatchEvent::Added(task),
            PreparedWatchEventKind::Modified => TaskWatchEvent::Modified(task),
            PreparedWatchEventKind::Deleted => TaskWatchEvent::Deleted(task),
        }
    }

    fn into_buffered(self) -> BufferedWatchEvent {
        let serialized_bytes = self.serialized_bytes;
        BufferedWatchEvent {
            event: self.materialize(),
            serialized_bytes,
        }
    }
}

struct BufferedWatchEvent {
    event: TaskWatchEvent,
    serialized_bytes: usize,
}

struct PreparedReplayChange {
    revision: u64,
    event: Option<BufferedWatchEvent>,
}

struct LagRecovery {
    target_revision: u64,
    pending: Option<RecoveryEventProbe>,
}

#[derive(Clone)]
struct RecoveryEventProbe {
    revision: u64,
    kind: PreparedWatchEventKind,
    serialized_bytes: usize,
    history_token: Arc<()>,
}

impl TaskWatchSubscription {
    fn matches(&self, task: &Task) -> bool {
        self.filter.matches(task) && (self.predicate)(task)
    }

    fn descriptor_for(&self, change: &RawTaskChange) -> Option<WatchEventDescriptor> {
        let previous_matches = change
            .previous
            .as_ref()
            .is_some_and(|task| self.matches(task));
        let current_matches = change
            .current
            .as_ref()
            .is_some_and(|task| self.matches(task));

        match (previous_matches, current_matches) {
            (false, true) => change.current.as_ref().map(|task| WatchEventDescriptor {
                kind: PreparedWatchEventKind::Added,
                task: Arc::clone(task),
                resource_version: None,
                serialized_bytes: TaskState::serialized_task_payload_bytes(
                    None,
                    Some(task.as_ref()),
                ),
            }),
            (true, true) => change.current.as_ref().map(|task| WatchEventDescriptor {
                kind: PreparedWatchEventKind::Modified,
                task: Arc::clone(task),
                resource_version: None,
                serialized_bytes: TaskState::serialized_task_payload_bytes(
                    None,
                    Some(task.as_ref()),
                ),
            }),
            (true, false) => {
                let task = change.previous.as_ref()?;
                let resource_version =
                    TaskState::format_resource_version(change.epoch.as_ref(), change.revision);
                Some(WatchEventDescriptor {
                    kind: PreparedWatchEventKind::Deleted,
                    task: Arc::clone(task),
                    serialized_bytes: TaskState::serialized_task_with_resource_version_bytes(
                        task.as_ref(),
                        &resource_version,
                    ),
                    resource_version: Some(resource_version),
                })
            }
            (false, false) => None,
        }
    }

    fn begin_recovery_after_lag(&mut self) -> Result<(), CollectionError> {
        let inner = self.inner.read();
        if inner.resource_version_epoch.as_ref() != self.epoch.as_ref() {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: TaskState::format_resource_version(
                    self.epoch.as_ref(),
                    self.last_revision,
                ),
            });
        }
        if self.last_revision < inner.compacted_through {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: TaskState::format_resource_version(
                    self.epoch.as_ref(),
                    self.last_revision,
                ),
            });
        }
        self.recovery = Some(LagRecovery {
            target_revision: inner.resource_version,
            pending: None,
        });
        Ok(())
    }

    fn next_recovery_change(
        &self,
        target_revision: u64,
    ) -> Result<Option<Arc<RawTaskChange>>, CollectionError> {
        if self.last_revision >= target_revision {
            return Ok(None);
        }
        let inner = self.inner.read();
        if inner.resource_version_epoch.as_ref() != self.epoch.as_ref()
            || self.last_revision < inner.compacted_through
        {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: TaskState::format_resource_version(
                    self.epoch.as_ref(),
                    self.last_revision,
                ),
            });
        }
        Ok(inner
            .watch_history
            .iter()
            .find(|change| {
                change.revision > self.last_revision && change.revision <= target_revision
            })
            .cloned())
    }

    fn descriptor_from_recovery_probe(
        &self,
        target_revision: u64,
        probe: &RecoveryEventProbe,
    ) -> Result<WatchEventDescriptor, CollectionError> {
        let change = self.next_recovery_change(target_revision)?.ok_or_else(|| {
            CollectionError::ResourceVersionExpired {
                resource_version: TaskState::format_resource_version(
                    self.epoch.as_ref(),
                    self.last_revision,
                ),
            }
        })?;
        if change.revision != probe.revision {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: TaskState::format_resource_version(
                    self.epoch.as_ref(),
                    self.last_revision,
                ),
            });
        }
        let (task, resource_version) = match probe.kind {
            PreparedWatchEventKind::Added | PreparedWatchEventKind::Modified => (
                change.current.as_ref().map(Arc::clone).ok_or_else(|| {
                    CollectionError::ResourceVersionExpired {
                        resource_version: TaskState::format_resource_version(
                            self.epoch.as_ref(),
                            self.last_revision,
                        ),
                    }
                })?,
                None,
            ),
            PreparedWatchEventKind::Deleted => (
                change.previous.as_ref().map(Arc::clone).ok_or_else(|| {
                    CollectionError::ResourceVersionExpired {
                        resource_version: TaskState::format_resource_version(
                            self.epoch.as_ref(),
                            self.last_revision,
                        ),
                    }
                })?,
                Some(TaskState::format_resource_version(
                    change.epoch.as_ref(),
                    change.revision,
                )),
            ),
        };
        Ok(WatchEventDescriptor {
            kind: probe.kind,
            task,
            resource_version,
            serialized_bytes: probe.serialized_bytes,
        })
    }

    fn take_buffered_event(&self, buffered: BufferedWatchEvent) -> TaskWatchEvent {
        if let Some(permit) = self.permit.as_ref() {
            permit.release_bytes(buffered.serialized_bytes);
        }
        buffered.event
    }

    fn finish_terminal(&mut self) {
        self.terminal = true;
        self.initial.clear();
        self.replay.clear();
        self.recovery = None;
        self.permit.take();
    }

    fn terminal_error(
        &mut self,
        error: CollectionError,
    ) -> Poll<Option<Result<TaskWatchEvent, CollectionError>>> {
        self.finish_terminal();
        Poll::Ready(Some(Err(error)))
    }
}

impl Stream for TaskWatchSubscription {
    type Item = Result<TaskWatchEvent, CollectionError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }
        if self.stop.as_mut().poll(cx).is_ready() {
            self.finish_terminal();
            return Poll::Ready(None);
        }

        if let Some(event) = self.initial.pop_front() {
            if self.initial.is_empty()
                && let Some(revision) = self.initial_revision.take()
            {
                self.last_revision = revision;
            }
            let event = self.take_buffered_event(event);
            return Poll::Ready(Some(Ok(event)));
        }
        if let Some(revision) = self.initial_revision.take() {
            self.last_revision = revision;
        }

        let mut processed = 0;
        loop {
            if processed == WATCH_POLL_BUDGET {
                if self.stop.as_mut().poll(cx).is_ready() {
                    self.finish_terminal();
                    return Poll::Ready(None);
                }
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            if let Some(change) = self.replay.pop_front() {
                processed += 1;
                self.last_revision = change.revision;
                if let Some(event) = change.event {
                    let event = self.take_buffered_event(event);
                    return Poll::Ready(Some(Ok(event)));
                }
                continue;
            }

            if self.recovery.is_some() {
                let target_revision = self
                    .recovery
                    .as_ref()
                    .expect("checked recovery state must be present")
                    .target_revision;
                if let Some(probe) = self
                    .recovery
                    .as_ref()
                    .expect("checked recovery state must be present")
                    .pending
                    .clone()
                {
                    let reservation = self
                        .permit
                        .as_ref()
                        .expect("an active watch must own an admission lease")
                        .poll_reserve_replay(probe.serialized_bytes, &probe.history_token, cx);
                    let reservation = match reservation {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(WatchReplayReservation::Closed) => {
                            self.finish_terminal();
                            return Poll::Ready(None);
                        }
                        Poll::Ready(WatchReplayReservation::HistoryChanged) => {
                            processed += 1;
                            let history_token = self
                                .permit
                                .as_ref()
                                .expect("an active watch must own an admission lease")
                                .history_token();
                            if let Err(error) =
                                self.descriptor_from_recovery_probe(target_revision, &probe)
                            {
                                return self.terminal_error(error);
                            }
                            self.recovery
                                .as_mut()
                                .expect("checked recovery state must be present")
                                .pending
                                .as_mut()
                                .expect("checked recovery probe must be present")
                                .history_token = history_token;
                            continue;
                        }
                        Poll::Ready(reservation) => reservation,
                    };
                    let descriptor =
                        match self.descriptor_from_recovery_probe(target_revision, &probe) {
                            Ok(descriptor) => descriptor,
                            Err(error) => {
                                if matches!(reservation, WatchReplayReservation::Reserved) {
                                    self.permit
                                        .as_ref()
                                        .expect("an active watch must own an admission lease")
                                        .release_bytes(probe.serialized_bytes);
                                }
                                return self.terminal_error(error);
                            }
                        };
                    self.recovery
                        .as_mut()
                        .expect("checked recovery state must be present")
                        .pending = None;
                    self.last_revision = probe.revision;
                    let event = descriptor.materialize();
                    if matches!(reservation, WatchReplayReservation::Reserved) {
                        self.permit
                            .as_ref()
                            .expect("an active watch must own an admission lease")
                            .release_bytes(probe.serialized_bytes);
                    }
                    return Poll::Ready(Some(Ok(event)));
                }

                let history_token = self
                    .permit
                    .as_ref()
                    .expect("an active watch must own an admission lease")
                    .history_token();
                let change = match self.next_recovery_change(target_revision) {
                    Ok(Some(change)) => change,
                    Ok(None) => {
                        self.last_revision = target_revision;
                        self.recovery = None;
                        continue;
                    }
                    Err(error) => return self.terminal_error(error),
                };
                processed += 1;
                let revision = change.revision;
                let Some(descriptor) = self.descriptor_for(&change) else {
                    self.last_revision = revision;
                    continue;
                };
                let probe = RecoveryEventProbe {
                    revision,
                    kind: descriptor.kind,
                    serialized_bytes: descriptor.serialized_bytes,
                    history_token,
                };
                let reservation = self
                    .permit
                    .as_ref()
                    .expect("an active watch must own an admission lease")
                    .poll_reserve_replay(descriptor.serialized_bytes, &probe.history_token, cx);
                match reservation {
                    Poll::Pending => {
                        self.recovery
                            .as_mut()
                            .expect("checked recovery state must be present")
                            .pending = Some(probe);
                        return Poll::Pending;
                    }
                    Poll::Ready(WatchReplayReservation::HistoryChanged) => {
                        self.recovery
                            .as_mut()
                            .expect("checked recovery state must be present")
                            .pending = Some(probe);
                        continue;
                    }
                    Poll::Ready(WatchReplayReservation::Closed) => {
                        self.finish_terminal();
                        return Poll::Ready(None);
                    }
                    Poll::Ready(WatchReplayReservation::Oversized) => {
                        self.last_revision = revision;
                        return Poll::Ready(Some(Ok(descriptor.materialize())));
                    }
                    Poll::Ready(WatchReplayReservation::Reserved) => {
                        let bytes = descriptor.serialized_bytes;
                        let event = descriptor.materialize();
                        self.last_revision = revision;
                        self.permit
                            .as_ref()
                            .expect("an active watch must own an admission lease")
                            .release_bytes(bytes);
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
            }

            match Pin::new(&mut self.receiver).poll_next(cx) {
                Poll::Ready(Some(Ok(change))) => {
                    processed += 1;
                    if change.epoch.as_ref() != self.epoch.as_ref() {
                        let resource_version = TaskState::format_resource_version(
                            self.epoch.as_ref(),
                            self.last_revision,
                        );
                        return self.terminal_error(CollectionError::ResourceVersionExpired {
                            resource_version,
                        });
                    }
                    if change.revision <= self.last_revision {
                        continue;
                    }
                    self.last_revision = change.revision;
                    if let Some(descriptor) = self.descriptor_for(&change) {
                        return Poll::Ready(Some(Ok(descriptor.materialize())));
                    }
                }
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => {
                    processed += 1;
                    if let Err(error) = self.begin_recovery_after_lag() {
                        return self.terminal_error(error);
                    }
                }
                Poll::Ready(None) => {
                    self.finish_terminal();
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Exact resource identity and generation for one runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResourceGeneration {
    pub(crate) name: TaskId,
    pub(crate) uid: Uid,
    pub(crate) generation: u64,
    pub(crate) workload: WorkloadTypeMeta,
}

impl ResourceGeneration {
    pub(crate) fn from_task(task: &Task) -> Self {
        Self {
            name: task.name().clone(),
            uid: task.uid().clone(),
            generation: task.metadata().generation(),
            workload: task.spec().workload().type_meta(),
        }
    }
}

/// Binding between Taskvisor and one task generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RuntimeBinding {
    pub(crate) resource: ResourceGeneration,
    pub(crate) tv: taskvisor::TaskId,
}

/// Result of a desired-state commit.
#[derive(Clone, Debug)]
pub(crate) struct DesiredCommit {
    pub(crate) task: Task,
    pub(crate) reconcile: bool,
}

impl TaskState {
    /// Creates empty state with default admission and retention settings.
    ///
    /// Most applications use [`SupervisorApi::state`](crate::SupervisorApi::state).
    /// Use [`try_new`](Self::try_new) when initialization failure must be handled.
    ///
    /// # Panics
    ///
    /// Panics when the resource-version epoch cannot be generated from OS entropy.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    ///
    /// let state = TaskState::new();
    /// assert!(state.list_all().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::try_new()
            .expect("OS entropy is required to create a TaskState resource-version epoch")
    }

    /// Tries to create empty state with default admission and retention settings.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StateInitialization`] when identity generation fails.
    pub fn try_new() -> Result<Self, CoreError> {
        Self::try_with_config(StateConfig::new())
    }

    pub(crate) fn try_with_config(config: StateConfig) -> Result<Self, CoreError> {
        Self::try_with_config_and_sink(config, None)
    }

    pub(crate) fn try_with_config_and_sink(
        config: StateConfig,
        event_sink: Option<TaskStateSinkHandle>,
    ) -> Result<Self, CoreError> {
        Self::try_with_config_sink_and_persistence(config, event_sink, PersistenceConfig::default())
    }

    pub(crate) fn try_with_config_sink_and_persistence(
        config: StateConfig,
        event_sink: Option<TaskStateSinkHandle>,
        persistence_config: PersistenceConfig,
    ) -> Result<Self, CoreError> {
        let epoch = Uid::generate()
            .map_err(CoreError::StateInitialization)?
            .to_string();
        Self::try_with_epoch_and_sink(config, epoch, event_sink, persistence_config)
    }

    #[cfg(test)]
    fn with_epoch(config: StateConfig, epoch: String) -> Self {
        Self::with_epoch_and_sink(config, epoch, None)
    }

    #[cfg(test)]
    fn with_epoch_and_sink(
        config: StateConfig,
        epoch: String,
        event_sink: Option<TaskStateSinkHandle>,
    ) -> Self {
        Self::try_with_epoch_and_sink(config, epoch, event_sink, PersistenceConfig::default())
            .expect("test persistence worker must start")
    }

    fn try_with_epoch_and_sink(
        config: StateConfig,
        epoch: String,
        event_sink: Option<TaskStateSinkHandle>,
        persistence_config: PersistenceConfig,
    ) -> Result<Self, CoreError> {
        let (watch_tx, _) =
            broadcast::channel(task_watch_live_capacity(config.watch_history_capacity()));
        let watch_admission = WatchAdmission::new(config);
        let run_resource_version_epoch = format!("runs-{epoch}");
        Ok(Self {
            inner: Arc::new(RwLock::new(TaskStateInner {
                by_slot: HashMap::new(),
                tasks: HashMap::new(),
                ordered_tasks: BTreeSet::new(),
                runs: HashMap::new(),
                run_resource_version_epoch,
                run_resource_version: 0,
                run_history: VecDeque::new(),
                run_history_bytes: 0,
                run_history_byte_budget: config.run_history_byte_budget(),
                run_compacted_through: 0,
                run_history_capacity: config.run_history_capacity(),
                by_tv: HashMap::new(),
                tv_of: HashMap::new(),
                finished_attempt_by_tv: HashMap::new(),
                resource_version_epoch: Arc::from(epoch),
                resource_version: 0,
                watch_history: VecDeque::new(),
                watch_history_bytes: 0,
                watch_history_byte_budget: config.watch_history_byte_budget(),
                compacted_through: 0,
                watch_history_capacity: config.watch_history_capacity(),
                watch_tx,
                terminal_since: HashMap::new(),
                max_runs_per_task: config.max_runs_per_task(),
                max_retained_tasks: config.max_retained_tasks(),
                retained_task_manifest_bytes: 0,
                retained_task_manifest_bytes_by_name: HashMap::new(),
                max_retained_task_manifest_bytes: config.max_retained_task_manifest_bytes(),
            })),
            watch_stop: CancellationToken::new(),
            watch_admission,
            event_publisher: Arc::new(
                StateEventPublisher::new(event_sink, persistence_config)
                    .map_err(CoreError::PersistenceInitialization)?,
            ),
        })
    }

    fn write(&self, event_capacity: StateMutationEventCapacity) -> TaskStateWriteGuard<'_> {
        // Reserve every event slot atomically before acquiring the authoritative
        // lock. A slow persistence sink therefore cannot extend the global
        // critical section or leave hidden, unpermitted events in a pending batch.
        let permits = self.event_publisher.reserve(event_capacity.get());
        let inner = self.inner.write();
        let batch = self.event_publisher.begin_batch(permits);
        TaskStateWriteGuard {
            inner: Some(inner),
            watch_admission: self.watch_admission.as_ref(),
            watch_history_invalidated: false,
            publisher: &self.event_publisher,
            batch,
        }
    }

    #[cfg(test)]
    fn set_max_runs_per_task(&self, max: usize) {
        self.write(StateMutationEventCapacity::None)
            .max_runs_per_task = max;
    }

    fn format_resource_version(epoch: &str, revision: u64) -> String {
        format!("{epoch}:{revision}")
    }

    fn current_resource_version(inner: &TaskStateInner) -> String {
        Self::format_resource_version(
            inner.resource_version_epoch.as_ref(),
            inner.resource_version,
        )
    }

    fn current_run_resource_version(inner: &TaskStateInner) -> String {
        Self::format_resource_version(
            &inner.run_resource_version_epoch,
            inner.run_resource_version,
        )
    }

    fn next_resource_version(inner: &mut TaskStateWriteGuard<'_>) -> (u64, String) {
        if inner.resource_version == u64::MAX {
            inner.invalidate_watch_history();
            inner.resource_version_epoch = Arc::from(Self::next_resource_version_epoch(
                inner.resource_version_epoch.as_ref(),
            ));
            inner.resource_version = 0;
            inner.watch_history.clear();
            inner.watch_history_bytes = 0;
            inner.compacted_through = 0;
        }
        inner.resource_version += 1;
        (
            inner.resource_version,
            Self::current_resource_version(inner),
        )
    }

    /// Advances an opaque store epoch without depending on entropy.
    fn next_resource_version_epoch(epoch: &str) -> String {
        format!("next-{epoch}")
    }

    fn serialized_task_payload_bytes(previous: Option<&Task>, current: Option<&Task>) -> usize {
        let mut counter = SerializedSizeCounter::default();
        for task in [previous, current].into_iter().flatten() {
            serde_json::to_writer(&mut counter, task)
                .expect("TaskState resources must serialize as JSON");
        }
        counter.0
    }

    fn serialized_json_string_bytes(value: &str) -> usize {
        let mut counter = SerializedSizeCounter::default();
        serde_json::to_writer(&mut counter, value)
            .expect("validated resource versions must serialize as JSON strings");
        counter.0
    }

    fn serialized_task_with_resource_version_bytes(task: &Task, resource_version: &str) -> usize {
        let task_bytes = Self::serialized_task_payload_bytes(None, Some(task));
        let previous_version_bytes =
            Self::serialized_json_string_bytes(task.metadata().resource_version());
        let next_version_bytes = Self::serialized_json_string_bytes(resource_version);
        task_bytes
            .checked_sub(previous_version_bytes)
            .and_then(|bytes| bytes.checked_add(next_version_bytes))
            .expect("Task JSON must contain its serialized resource version exactly once")
    }

    /// Returns the compact JSON size of one TaskRun page item.
    fn serialized_run_payload_bytes(run: &TaskRun) -> usize {
        let mut counter = SerializedSizeCounter::default();
        serde_json::to_writer(&mut counter, run).expect("TaskRun values must serialize as JSON");
        counter.0
    }

    /// Returns the journal charge for one TaskRun mutation batch.
    fn serialized_run_change_bytes(changes: &[RawRunChange]) -> usize {
        let mut counter = SerializedSizeCounter::default();
        for change in changes {
            serde_json::to_writer(&mut counter, &change.task)
                .expect("TaskId values must serialize as JSON");
            serde_json::to_writer(&mut counter, &change.task_uid)
                .expect("Uid values must serialize as JSON");
            for run in [change.previous.as_ref(), change.current.as_ref()]
                .into_iter()
                .flatten()
            {
                serde_json::to_writer(&mut counter, run.as_ref())
                    .expect("TaskRun values must serialize as JSON");
            }
        }
        counter.0
    }

    /// Commits one reversible TaskRun revision and enforces journal limits.
    fn record_run_snapshot_changes(inner: &mut TaskStateInner, mut changes: Vec<RawRunChange>) {
        changes.retain(|change| change.previous != change.current);
        if changes.is_empty() {
            return;
        }

        if inner.run_resource_version == u64::MAX {
            inner.run_resource_version_epoch =
                Self::next_resource_version_epoch(&inner.run_resource_version_epoch);
            inner.run_resource_version = 0;
            inner.run_history.clear();
            inner.run_history_bytes = 0;
            inner.run_compacted_through = 0;
        }
        inner.run_resource_version += 1;
        let revision = inner.run_resource_version;
        let serialized_bytes = Self::serialized_run_change_bytes(&changes);
        let batch = RawRunChangeBatch {
            revision,
            changes,
            serialized_bytes,
        };

        if serialized_bytes > inner.run_history_byte_budget {
            inner.run_history.clear();
            inner.run_history_bytes = 0;
            inner.run_compacted_through = revision;
            return;
        }

        while inner.run_history.len() >= inner.run_history_capacity
            || inner.run_history_bytes > inner.run_history_byte_budget - serialized_bytes
        {
            let compacted = inner
                .run_history
                .pop_front()
                .expect("a non-empty TaskRun journal must satisfy its configured limits");
            inner.run_history_bytes = inner
                .run_history_bytes
                .checked_sub(compacted.serialized_bytes)
                .expect("TaskRun journal byte accounting must not underflow");
            inner.run_compacted_through = compacted.revision;
        }
        inner.run_history_bytes = inner
            .run_history_bytes
            .checked_add(serialized_bytes)
            .expect("TaskRun journal byte accounting must not overflow");
        inner.run_history.push_back(batch);
    }

    /// Creates one reversible TaskRun change for a task identity.
    fn run_snapshot_change(
        task: &TaskId,
        task_uid: &Uid,
        previous: Option<Arc<TaskRun>>,
        current: Option<Arc<TaskRun>>,
    ) -> RawRunChange {
        RawRunChange {
            task: task.clone(),
            task_uid: task_uid.clone(),
            previous,
            current,
        }
    }

    /// Returns the canonical compact JSON bytes of one caller-owned manifest.
    fn serialized_task_manifest_bytes(manifest: &TaskManifest) -> usize {
        let mut counter = SerializedSizeCounter::default();
        serde_json::to_writer(&mut counter, manifest)
            .expect("validated TaskManifest resources must serialize as JSON");
        counter.0
    }

    fn record_change(
        &self,
        inner: &mut TaskStateWriteGuard<'_>,
        revision: u64,
        previous: Option<Arc<Task>>,
        current: Option<Arc<Task>>,
    ) {
        if previous == current {
            return;
        }
        let persistence_event = TaskStateEvent::TaskChanged {
            resource_version: Self::format_resource_version(
                inner.resource_version_epoch.as_ref(),
                revision,
            ),
            previous: previous.clone(),
            current: current.clone(),
        };
        let serialized_bytes =
            Self::serialized_task_payload_bytes(previous.as_deref(), current.as_deref());
        let change = Arc::new(RawTaskChange {
            epoch: Arc::clone(&inner.resource_version_epoch),
            revision,
            previous,
            current,
            serialized_bytes,
        });

        let mut history_invalidated = false;
        if serialized_bytes > inner.watch_history_byte_budget {
            inner.watch_history.clear();
            inner.watch_history_bytes = 0;
            inner.compacted_through = revision;
            history_invalidated = true;
        } else {
            while inner.watch_history.len() >= inner.watch_history_capacity
                || inner.watch_history_bytes > inner.watch_history_byte_budget - serialized_bytes
            {
                let compacted = inner
                    .watch_history
                    .pop_front()
                    .expect("a non-empty watch history must satisfy its configured limits");
                inner.watch_history_bytes = inner
                    .watch_history_bytes
                    .checked_sub(compacted.serialized_bytes)
                    .expect("watch history byte accounting must not underflow");
                inner.compacted_through = compacted.revision;
                history_invalidated = true;
            }
            inner.watch_history_bytes = inner
                .watch_history_bytes
                .checked_add(serialized_bytes)
                .expect("watch history byte accounting must not overflow");
            inner.watch_history.push_back(Arc::clone(&change));
        }
        if history_invalidated {
            inner.invalidate_watch_history();
        }

        let _ = inner.watch_tx.send(change);
        inner.enqueue(persistence_event);
    }

    fn record_run_change(
        &self,
        inner: &TaskStateWriteGuard<'_>,
        task: &TaskId,
        task_uid: &Uid,
        run: &TaskRun,
    ) {
        inner.enqueue(TaskStateEvent::RunChanged {
            task: task.clone(),
            task_uid: task_uid.clone(),
            run: run.clone(),
        });
    }

    fn index_task(inner: &mut TaskStateInner, task: &Task) {
        inner.ordered_tasks.insert(task.name().clone());
        let ids = inner.by_slot.entry(task.slot().clone()).or_default();
        ids.insert(task.name().clone());
    }

    fn unindex_task(inner: &mut TaskStateInner, task: &Task) {
        inner.ordered_tasks.remove(task.name());
        if let Some(ids) = inner.by_slot.get_mut(task.slot()) {
            ids.remove(task.name());
            if ids.is_empty() {
                inner.by_slot.remove(task.slot());
            }
        }
    }

    /// Replaces one task's manifest-byte contribution.
    fn set_retained_task_manifest_bytes(
        inner: &mut TaskStateInner,
        name: &TaskId,
        manifest_bytes: usize,
    ) {
        let previous = inner
            .retained_task_manifest_bytes_by_name
            .insert(name.clone(), manifest_bytes)
            .unwrap_or(0);
        inner.retained_task_manifest_bytes = inner
            .retained_task_manifest_bytes
            .checked_sub(previous)
            .and_then(|current| current.checked_add(manifest_bytes))
            .expect("retained TaskManifest byte accounting must remain exact");
    }

    /// Removes one task's manifest-byte contribution.
    fn remove_retained_task_manifest_bytes(inner: &mut TaskStateInner, name: &TaskId) {
        let removed = inner
            .retained_task_manifest_bytes_by_name
            .remove(name)
            .expect("every retained task must have manifest byte accounting");
        inner.retained_task_manifest_bytes = inner
            .retained_task_manifest_bytes
            .checked_sub(removed)
            .expect("retained TaskManifest byte accounting must not underflow");
    }

    /// Verifies that one new task fits in the configured state limit.
    fn ensure_retained_task_capacity(inner: &TaskStateInner) -> Result<(), CoreError> {
        if let Some(limit) = inner.max_retained_tasks
            && inner.tasks.len() >= limit.get()
        {
            return Err(CoreError::RetainedTaskLimitReached { limit: limit.get() });
        }
        Ok(())
    }

    /// Verifies that one positive manifest-byte addition fits the state budget.
    fn ensure_retained_task_manifest_byte_capacity(
        inner: &TaskStateInner,
        requested: usize,
    ) -> Result<(), CoreError> {
        debug_assert!(requested > 0);
        if let Some(limit) = inner.max_retained_task_manifest_bytes
            && requested
                > limit
                    .get()
                    .saturating_sub(inner.retained_task_manifest_bytes)
        {
            return Err(CoreError::RetainedTaskManifestByteLimitExceeded {
                current: inner.retained_task_manifest_bytes,
                requested,
                limit: limit.get(),
            });
        }
        Ok(())
    }

    /// Inserts a manifest for tests.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn add_task(&self, manifest: TaskManifest) {
        let manifest_bytes = Self::serialized_task_manifest_bytes(&manifest);
        let name = manifest.name().clone();
        let mut task = Task::from_manifest(manifest).expect("test manifest must be valid");
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        let previous = inner.tasks.remove(&name);
        if let Some(previous) = previous.as_ref() {
            Self::unindex_task(&mut inner, previous);
            if let Some(runs) = inner.runs.remove(&name) {
                let changes = runs
                    .into_iter()
                    .map(|run| Self::run_snapshot_change(&name, previous.uid(), Some(run), None))
                    .collect();
                Self::record_run_snapshot_changes(&mut inner, changes);
            }
        }
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        task.set_resource_version(resource_version)
            .expect("store resource version must be valid");
        Self::index_task(&mut inner, &task);
        let task = Arc::new(task);
        inner.tasks.insert(name, Arc::clone(&task));
        Self::set_retained_task_manifest_bytes(&mut inner, task.name(), manifest_bytes);
        self.record_change(&mut inner, revision, previous, Some(task));
    }

    /// Creates one desired resource.
    ///
    /// Every retained name conflicts.
    /// A new name is rejected when the retained task limit is full.
    /// The task-count limit is checked before the manifest byte budget.
    /// No task is evicted or changed by a rejection.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::AlreadyExists`] when the name is retained.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when no task slot remains.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when the
    /// caller-owned manifest would exceed the aggregate byte budget.
    pub(crate) fn create_desired(
        &self,
        manifest: &TaskManifest,
    ) -> Result<DesiredCommit, CoreError> {
        let manifest_bytes = Self::serialized_task_manifest_bytes(manifest);
        let mut task = Task::from_manifest(manifest.clone())?;
        let name = manifest.name().clone();
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        if inner.tasks.contains_key(&name) {
            return Err(CoreError::AlreadyExists(format!(
                "Task resource '{name}' already exists"
            )));
        }
        Self::ensure_retained_task_capacity(&inner)?;
        Self::ensure_retained_task_manifest_byte_capacity(&inner, manifest_bytes)?;

        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        task.set_resource_version(resource_version)?;
        Self::index_task(&mut inner, &task);
        inner.terminal_since.remove(&name);
        let task = Arc::new(task);
        inner.tasks.insert(name, Arc::clone(&task));
        Self::set_retained_task_manifest_bytes(&mut inner, task.name(), manifest_bytes);
        self.record_change(&mut inner, revision, None, Some(Arc::clone(&task)));
        Ok(DesiredCommit {
            task: task.as_ref().clone(),
            reconcile: true,
        })
    }

    /// Applies a manifest by stable name.
    ///
    /// Missing state is created.
    #[cfg(test)]
    pub(crate) fn apply_desired(
        &self,
        manifest: &TaskManifest,
    ) -> Result<DesiredCommit, CoreError> {
        self.apply_desired_with_preconditions(manifest, &WritePreconditions::new())
    }

    /// Applies a manifest after checking write preconditions.
    ///
    /// Existing state can change when the retained task limit is full.
    /// Positive manifest growth is rejected when it exceeds the aggregate byte
    /// budget. Shrinks and equal-size writes remain allowed.
    /// An unchecked missing name uses create admission.
    /// A checked missing name remains not found.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] for checked missing state.
    /// Returns [`CoreError::RetainedTaskLimitReached`] when an unchecked missing
    /// name cannot be retained.
    /// Returns [`CoreError::RetainedTaskManifestByteLimitExceeded`] when a new
    /// manifest or positive existing-manifest growth exceeds the byte budget.
    pub(crate) fn apply_desired_with_preconditions(
        &self,
        manifest: &TaskManifest,
        preconditions: &WritePreconditions,
    ) -> Result<DesiredCommit, CoreError> {
        let desired_manifest_bytes = Self::serialized_task_manifest_bytes(manifest);
        let desired_labels = manifest.metadata().labels().clone();
        let desired_annotations = manifest.metadata().annotations().clone();
        let desired_spec = manifest.spec().clone();
        let name = manifest.name().clone();
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        let Some(current) = inner.tasks.get(&name) else {
            if !preconditions.is_empty() {
                return Err(CoreError::NotFound(name.to_string()));
            }
            drop(inner);
            return self.create_desired(manifest);
        };
        Self::check_write_preconditions(current, preconditions)?;
        let current_manifest_bytes = *inner
            .retained_task_manifest_bytes_by_name
            .get(&name)
            .expect("every retained task must have manifest byte accounting");

        let metadata_changed = current.metadata().labels() != &desired_labels
            || current.metadata().annotations() != &desired_annotations;
        let spec_changed = current.spec() != &desired_spec;
        let retry = !metadata_changed && !spec_changed && current.status().reconciliation_failed();
        if !metadata_changed && !spec_changed && !retry {
            return Ok(DesiredCommit {
                task: current.as_ref().clone(),
                reconcile: false,
            });
        }
        if desired_manifest_bytes > current_manifest_bytes {
            Self::ensure_retained_task_manifest_byte_capacity(
                &inner,
                desired_manifest_bytes - current_manifest_bytes,
            )?;
        }

        let previous = Arc::clone(current);
        let previous_slot = previous.slot().clone();
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        let task = inner
            .tasks
            .get_mut(&name)
            .expect("resource was checked under the same write lock");
        let task = Arc::make_mut(task);
        let change = task.apply_desired(
            desired_labels,
            desired_annotations,
            desired_spec,
            resource_version.clone(),
        )?;
        if retry {
            task.mark_reconciliation_pending(resource_version)?;
        }

        let task = inner
            .tasks
            .get(&name)
            .expect("applied resource must remain stored")
            .clone();
        if task.slot() != &previous_slot {
            if let Some(ids) = inner.by_slot.get_mut(&previous_slot) {
                ids.remove(&name);
                if ids.is_empty() {
                    inner.by_slot.remove(&previous_slot);
                }
            }
            Self::index_task(&mut inner, &task);
        }
        if change == DesiredChange::Spec || retry {
            inner.terminal_since.remove(&name);
        }
        Self::set_retained_task_manifest_bytes(&mut inner, &name, desired_manifest_bytes);
        self.record_change(
            &mut inner,
            revision,
            Some(previous),
            Some(Arc::clone(&task)),
        );
        Ok(DesiredCommit {
            task: task.as_ref().clone(),
            reconcile: change == DesiredChange::Spec || retry,
        })
    }

    pub(crate) fn check_write_preconditions(
        task: &Task,
        preconditions: &WritePreconditions,
    ) -> Result<(), CoreError> {
        let mut violations = Vec::with_capacity(2);
        if let Some(expected) = preconditions.uid()
            && expected != task.uid()
        {
            violations.push(WritePreconditionViolation::Uid {
                expected: expected.clone(),
                actual: task.uid().clone(),
            });
        }
        if let Some(expected) = preconditions.resource_version()
            && expected != task.metadata().resource_version()
        {
            violations.push(WritePreconditionViolation::ResourceVersion {
                expected: expected.to_owned(),
                actual: task.metadata().resource_version().to_owned(),
            });
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Conflict(WriteConflict::new(
                task.name().clone(),
                violations,
            )))
        }
    }

    /// Binds a Taskvisor submission to one resource generation.
    pub(crate) fn bind_tv(&self, resource: ResourceGeneration, tv: taskvisor::TaskId) -> bool {
        let mut inner = self.write(StateMutationEventCapacity::None);
        let current = inner.tasks.get(&resource.name).is_some_and(|task| {
            task.uid() == &resource.uid && task.metadata().generation() == resource.generation
        });
        if !current {
            return false;
        }
        if let Some(old) = inner.tv_of.remove(&resource.name) {
            inner.by_tv.remove(&old.tv.get());
            inner.finished_attempt_by_tv.remove(&old.tv.get());
        }
        let binding = RuntimeBinding { resource, tv };
        inner
            .tv_of
            .insert(binding.resource.name.clone(), binding.clone());
        inner.by_tv.insert(tv.get(), binding);
        true
    }

    /// Returns whether a resource generation is current.
    pub(crate) fn is_current(&self, resource: &ResourceGeneration) -> bool {
        self.inner
            .read()
            .tasks
            .get(&resource.name)
            .is_some_and(|task| {
                task.uid() == &resource.uid && task.metadata().generation() == resource.generation
            })
    }

    /// Resolves a Taskvisor identity to its binding.
    pub(crate) fn resolve_tv(&self, tv: u64) -> Option<RuntimeBinding> {
        self.inner.read().by_tv.get(&tv).cloned()
    }

    /// Returns the current binding for a resource name.
    pub(crate) fn binding_for(&self, name: &TaskId) -> Option<RuntimeBinding> {
        self.inner.read().tv_of.get(name).cloned()
    }

    fn unbind_locked(inner: &mut TaskStateInner, name: &TaskId) {
        if let Some(binding) = inner.tv_of.remove(name) {
            inner.by_tv.remove(&binding.tv.get());
            inner.finished_attempt_by_tv.remove(&binding.tv.get());
        }
    }

    pub(crate) fn unbind_tv(&self, tv_raw: u64) -> Option<RuntimeBinding> {
        let mut inner = self.write(StateMutationEventCapacity::None);
        let binding = inner.by_tv.get(&tv_raw)?.clone();
        if inner
            .tv_of
            .get(&binding.resource.name)
            .is_some_and(|current| current == &binding)
        {
            inner.tv_of.remove(&binding.resource.name);
        }
        inner.by_tv.remove(&tv_raw);
        inner.finished_attempt_by_tv.remove(&tv_raw);
        Some(binding)
    }

    /// Deletes a task and its run history.
    ///
    /// Returns `true` when the task existed.
    /// Reconciliation failures do not use this path.
    pub(crate) fn delete_task(&self, id: &TaskId) -> bool {
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        let task_uid = inner.tasks.get(id).map(|task| task.uid().clone());
        let removed_runs = inner.runs.remove(id);
        if let (Some(task_uid), Some(removed_runs)) = (task_uid.as_ref(), removed_runs) {
            let changes = removed_runs
                .into_iter()
                .map(|run| Self::run_snapshot_change(id, task_uid, Some(run), None))
                .collect();
            Self::record_run_snapshot_changes(&mut inner, changes);
        }
        inner.terminal_since.remove(id);

        Self::unbind_locked(&mut inner, id);
        if let Some(task) = inner.tasks.remove(id) {
            Self::unindex_task(&mut inner, &task);
            Self::remove_retained_task_manifest_bytes(&mut inner, id);
            let (revision, _) = Self::next_resource_version(&mut inner);
            self.record_change(&mut inner, revision, Some(task), None);
            true
        } else {
            false
        }
    }

    fn resource_matches(task: &Task, resource: &ResourceGeneration) -> bool {
        task.name() == &resource.name && task.uid() == &resource.uid
    }

    /// Removes the oldest completed runs above the configured completed-run cap.
    fn enforce_run_cap(runs: &mut VecDeque<Arc<TaskRun>>, max: usize) -> Vec<Arc<TaskRun>> {
        let mut removed = Vec::new();
        let mut completed = runs.iter().filter(|run| !run.is_active()).count();
        while completed > max {
            let Some(oldest_finished) = runs.iter().position(|run| !run.is_active()) else {
                break;
            };
            removed.push(
                runs.remove(oldest_finished)
                    .expect("the selected TaskRun index must exist"),
            );
            completed -= 1;
        }
        removed
    }

    /// Records an authoritative attempt start.
    pub(crate) fn transition_attempt_starting(
        &self,
        binding: &RuntimeBinding,
        attempt: u32,
    ) -> bool {
        if attempt == 0 {
            return false;
        }
        let mut inner = self.write(StateMutationEventCapacity::AttemptTransition);
        let name = &binding.resource.name;
        let Some(task) = inner.tasks.get(name) else {
            return false;
        };
        if !Self::resource_matches(task, &binding.resource) {
            return false;
        }
        let tv_raw = binding.tv.get();
        if inner
            .finished_attempt_by_tv
            .get(&tv_raw)
            .is_some_and(|finished| attempt <= *finished)
        {
            return false;
        }
        if inner.runs.get(name).is_some_and(|runs| {
            runs.iter().any(|run| {
                run.generation() == binding.resource.generation && run.attempt() >= attempt
            })
        }) {
            return false;
        }
        let updates_current_status = task.metadata().generation() == binding.resource.generation;
        if updates_current_status && attempt <= task.status().attempt() {
            return false;
        }

        let mut task_change = None;
        if updates_current_status {
            let previous = inner
                .tasks
                .get(name)
                .expect("resource was checked under the same write lock")
                .clone();
            let (revision, resource_version) = Self::next_resource_version(&mut inner);
            let task = inner
                .tasks
                .get_mut(name)
                .expect("resource was checked under the same write lock");
            let task = Arc::make_mut(task);
            match task.transition_starting(binding.resource.generation, attempt, resource_version) {
                Ok(true) => {
                    let current = inner
                        .tasks
                        .get(name)
                        .expect("resource was checked under the same write lock")
                        .clone();
                    task_change = Some((revision, previous, current));
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        event = "task.state_transition_rejected",
                        task_name = %name,
                        task_uid = %binding.resource.uid,
                        generation = binding.resource.generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        operation = "attempt_start",
                        %error,
                        "illegal state transition ignored"
                    );
                    return false;
                }
            }
            inner.terminal_since.remove(name);
        }

        let max_runs = inner.max_runs_per_task;
        let runs = inner.runs.entry(name.clone()).or_default();
        let mut run_changes = Vec::new();
        let mut run_snapshot_changes = Vec::new();
        for run in runs.iter_mut().filter(|run| {
            run.is_active()
                && run.generation() == binding.resource.generation
                && run.attempt() < attempt
        }) {
            let previous = Arc::clone(run);
            Arc::make_mut(run)
                .finish(
                    TaskPhase::Failed,
                    Some("run outcome not observed (a later attempt started first)".to_string()),
                    None,
                )
                .expect("an active run accepts a terminal phase");
            run_changes.push(Arc::clone(run));
            run_snapshot_changes.push(Self::run_snapshot_change(
                name,
                &binding.resource.uid,
                Some(previous),
                Some(Arc::clone(run)),
            ));
        }
        assert!(
            run_changes.len() <= 1,
            "TaskState must retain at most one active run for one generation"
        );
        let run = Arc::new(
            TaskRun::starting(
                binding.resource.generation,
                attempt,
                binding.resource.workload.clone(),
            )
            .expect("validated resource generation and attempt create a run"),
        );
        run_changes.push(Arc::clone(&run));
        run_snapshot_changes.push(Self::run_snapshot_change(
            name,
            &binding.resource.uid,
            None,
            Some(Arc::clone(&run)),
        ));
        runs.push_back(run);

        for removed in Self::enforce_run_cap(runs, max_runs) {
            run_snapshot_changes.push(Self::run_snapshot_change(
                name,
                &binding.resource.uid,
                Some(removed),
                None,
            ));
        }
        Self::record_run_snapshot_changes(&mut inner, run_snapshot_changes);
        if let Some((revision, previous, current)) = task_change {
            self.record_change(&mut inner, revision, Some(previous), Some(current));
        }
        for run in &run_changes {
            self.record_run_change(&inner, name, &binding.resource.uid, run.as_ref());
        }
        true
    }

    /// Closes the attempt described by a Taskvisor event.
    pub(crate) fn transition_attempt_finished(
        &self,
        binding: &RuntimeBinding,
        attempt: u32,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
        if attempt == 0 || !phase.is_terminal() {
            return false;
        }
        let mut inner = self.write(StateMutationEventCapacity::AttemptTransition);
        let name = &binding.resource.name;
        let Some(task) = inner.tasks.get(name) else {
            return false;
        };
        if !Self::resource_matches(task, &binding.resource) {
            return false;
        }
        let tv_raw = binding.tv.get();
        if inner
            .finished_attempt_by_tv
            .get(&tv_raw)
            .is_some_and(|finished| attempt <= *finished)
        {
            return false;
        }
        let updates_current_status = task.metadata().generation() == binding.resource.generation
            && attempt >= task.status().attempt();

        let max_runs = inner.max_runs_per_task;
        let (run_error, run_exit_code, run_changed, run_changes, run_snapshot_changes) = {
            let runs = inner.runs.entry(name.clone()).or_default();
            let mut run_changes = Vec::new();
            let mut run_snapshot_changes = Vec::new();
            for previous in runs.iter_mut().filter(|run| {
                run.is_active()
                    && run.generation() == binding.resource.generation
                    && run.attempt() < attempt
            }) {
                let active = Arc::clone(previous);
                Arc::make_mut(previous)
                    .finish(
                        TaskPhase::Failed,
                        Some(
                            "run outcome not observed (a later attempt finished first)".to_string(),
                        ),
                        None,
                    )
                    .expect("an active run accepts a terminal phase");
                run_changes.push(Arc::clone(previous));
                run_snapshot_changes.push(Self::run_snapshot_change(
                    name,
                    &binding.resource.uid,
                    Some(active),
                    Some(Arc::clone(previous)),
                ));
            }
            assert!(
                run_changes.len() <= 1,
                "TaskState must retain at most one active run for one generation"
            );
            let index = runs.iter().position(|run| {
                run.generation() == binding.resource.generation && run.attempt() == attempt
            });
            let previous = index.map(|index| Arc::clone(&runs[index]));
            let run = if let Some(index) = index {
                &mut runs[index]
            } else {
                runs.push_back(Arc::new(
                    TaskRun::starting(
                        binding.resource.generation,
                        attempt,
                        binding.resource.workload.clone(),
                    )
                    .expect("validated resource generation and attempt create a run"),
                ));
                runs.back_mut().expect("the run was just appended")
            };
            let run_changed = run.is_active();
            if run_changed {
                Arc::make_mut(run)
                    .finish(phase, error, exit_code)
                    .expect("terminal phase closes an active run");
                run_changes.push(Arc::clone(run));
                run_snapshot_changes.push(Self::run_snapshot_change(
                    name,
                    &binding.resource.uid,
                    previous,
                    Some(Arc::clone(run)),
                ));
            }
            let run_error = run.error().map(str::to_owned);
            let run_exit_code = run.exit_code();
            for removed in Self::enforce_run_cap(runs, max_runs) {
                run_snapshot_changes.push(Self::run_snapshot_change(
                    name,
                    &binding.resource.uid,
                    Some(removed),
                    None,
                ));
            }
            (
                run_error,
                run_exit_code,
                run_changed,
                run_changes,
                run_snapshot_changes,
            )
        };
        Self::record_run_snapshot_changes(&mut inner, run_snapshot_changes);

        let mut status_changed = false;
        let mut task_change = None;
        if updates_current_status {
            let previous = inner
                .tasks
                .get(name)
                .expect("resource was checked under the same write lock")
                .clone();
            let (revision, resource_version) = Self::next_resource_version(&mut inner);
            let task = inner
                .tasks
                .get_mut(name)
                .expect("resource was checked under the same write lock");
            let task = Arc::make_mut(task);
            status_changed = match task.transition_finished(
                binding.resource.generation,
                attempt,
                phase,
                run_error,
                run_exit_code,
                resource_version,
            ) {
                Ok(changed) => changed,
                Err(error) => {
                    tracing::warn!(
                        event = "task.state_transition_rejected",
                        task_name = %name,
                        task_uid = %binding.resource.uid,
                        generation = binding.resource.generation,
                        taskvisor_id = tv_raw,
                        attempt,
                        operation = "attempt_finish",
                        %error,
                        "illegal state transition ignored"
                    );
                    return false;
                }
            };
            if status_changed {
                let current = inner
                    .tasks
                    .get(name)
                    .expect("resource was checked under the same write lock")
                    .clone();
                task_change = Some((revision, previous, current));
            }
            if status_changed
                && inner
                    .tasks
                    .get(name)
                    .is_some_and(|task| task.status().phase().is_terminal())
            {
                inner.terminal_since.insert(name.clone(), SystemTime::now());
            }
        }
        let changed = run_changed || status_changed;
        if changed {
            inner.finished_attempt_by_tv.insert(tv_raw, attempt);
        }
        if let Some((revision, previous, current)) = task_change {
            self.record_change(&mut inner, revision, Some(previous), Some(current));
        }
        for run in &run_changes {
            self.record_run_change(&inner, name, &binding.resource.uid, run.as_ref());
        }
        changed
    }

    /// Projects a task-level final event.
    ///
    /// No attempt number is invented.
    pub(crate) fn transition_task_finished(
        &self,
        binding: &RuntimeBinding,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) -> bool {
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        self.transition_task_finished_locked(&mut inner, binding, phase, error, exit_code, false)
    }

    /// Marks a generation as accepted by Taskvisor intake.
    pub(crate) fn mark_observed(&self, resource: &ResourceGeneration) -> bool {
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        let Some(task) = inner.tasks.get(&resource.name) else {
            return false;
        };
        if !Self::resource_matches(task, resource)
            || task.metadata().generation() != resource.generation
        {
            return false;
        }
        let previous = task.clone();
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        let changed = inner
            .tasks
            .get_mut(&resource.name)
            .map(Arc::make_mut)
            .expect("resource was checked under the same write lock")
            .mark_observed(resource_version)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    event = "task.state_transition_rejected",
                    task_name = %resource.name,
                    task_uid = %resource.uid,
                    generation = resource.generation,
                    operation = "mark_observed",
                    %error,
                    "state transition rejected"
                );
                false
            });
        if changed {
            let current = inner
                .tasks
                .get(&resource.name)
                .expect("resource was checked under the same write lock")
                .clone();
            self.record_change(&mut inner, revision, Some(previous), Some(current));
        }
        changed
    }

    /// Records a reconciliation failure.
    ///
    /// Desired state remains retained.
    pub(crate) fn mark_reconciliation_failed(
        &self,
        resource: &ResourceGeneration,
        reason: &'static str,
        message: String,
    ) -> bool {
        let mut inner = self.write(StateMutationEventCapacity::TaskChange);
        let Some(task) = inner.tasks.get(&resource.name) else {
            return false;
        };
        if !Self::resource_matches(task, resource)
            || task.metadata().generation() != resource.generation
        {
            return false;
        }
        let previous = task.clone();
        let (revision, resource_version) = Self::next_resource_version(&mut inner);
        let changed = inner
            .tasks
            .get_mut(&resource.name)
            .map(Arc::make_mut)
            .expect("resource was checked under the same write lock")
            .mark_reconciliation_failed(reason, message, resource_version)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    event = "task.state_transition_rejected",
                    task_name = %resource.name,
                    task_uid = %resource.uid,
                    generation = resource.generation,
                    operation = "record_reconciliation_failure",
                    reason,
                    %error,
                    "state transition rejected"
                );
                false
            });
        if changed {
            let current = inner
                .tasks
                .get(&resource.name)
                .expect("resource was checked under the same write lock")
                .clone();
            self.record_change(&mut inner, revision, Some(previous), Some(current));
        }
        changed
    }

    fn transition_task_finished_locked(
        &self,
        inner: &mut TaskStateWriteGuard<'_>,
        binding: &RuntimeBinding,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        force: bool,
    ) -> bool {
        let name = &binding.resource.name;
        let Some(task) = inner.tasks.get(name) else {
            return false;
        };
        if !Self::resource_matches(task, &binding.resource)
            || task.metadata().generation() != binding.resource.generation
        {
            return false;
        }

        let current_phase = task.status().phase();
        let preserve_timeout =
            force && current_phase == TaskPhase::Timeout && phase == TaskPhase::Exhausted;
        let refines_failed = current_phase == TaskPhase::Failed
            && matches!(phase, TaskPhase::Exhausted | TaskPhase::Timeout);
        if preserve_timeout || (!force && current_phase.is_terminal() && !refines_failed) {
            return true;
        }

        let previous = task.clone();
        let (revision, resource_version) = Self::next_resource_version(inner);
        let task = inner
            .tasks
            .get_mut(name)
            .expect("resource was checked under the same write lock");
        let result = Arc::make_mut(task).reconcile_finished(
            binding.resource.generation,
            phase,
            error.clone(),
            exit_code,
            resource_version,
        );
        match result {
            Ok(changed) => {
                inner.terminal_since.insert(name.clone(), SystemTime::now());
                if changed {
                    let current = inner
                        .tasks
                        .get(name)
                        .expect("resource was checked under the same write lock")
                        .clone();
                    self.record_change(inner, revision, Some(previous), Some(current));
                }
                true
            }
            Err(error) => {
                tracing::warn!(
                    event = "task.state_transition_rejected",
                    task_name = %name,
                    task_uid = %binding.resource.uid,
                    generation = binding.resource.generation,
                    taskvisor_id = binding.tv.get(),
                    operation = "task_finalize",
                    %error,
                    "illegal state transition ignored"
                );
                false
            }
        }
    }

    /// Finalizes the entry bound to a Taskvisor identity.
    ///
    /// Binding checks and mutations use one write lock.
    /// A stale waiter cannot touch a newer UID or generation.
    /// Finalization always releases the exact binding.
    ///
    /// Event-derived terminal phases normally remain sticky.
    /// The model permits `Failed` refinement to `Exhausted` or `Timeout`.
    /// A concrete attempt timeout remains more specific than generic exhaustion.
    ///
    /// Returns the bound task name.
    /// Returns `None` for a stale binding.
    pub(crate) fn finalize_if_bound(
        &self,
        tv_raw: u64,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
        force: bool,
    ) -> Option<TaskId> {
        if !phase.is_terminal() {
            return None;
        }
        let mut inner = self.write(StateMutationEventCapacity::TaskAndRunChange);

        let binding = inner.by_tv.get(&tv_raw)?.clone();
        if inner
            .tv_of
            .get(&binding.resource.name)
            .is_none_or(|current| current != &binding)
        {
            return None;
        }
        inner.tv_of.remove(&binding.resource.name);
        inner.by_tv.remove(&tv_raw);
        inner.finished_attempt_by_tv.remove(&tv_raw);

        let max_runs = inner.max_runs_per_task;
        let mut run_change = None;
        let mut run_snapshot_changes = Vec::new();
        if let Some(runs) = inner.runs.get_mut(&binding.resource.name) {
            if let Some(run) = runs
                .iter_mut()
                .rev()
                .find(|run| run.generation() == binding.resource.generation && run.is_active())
            {
                let previous = Arc::clone(run);
                Arc::make_mut(run)
                    .finish(phase, error.clone(), exit_code)
                    .expect("terminal phase closes an active run");
                run_change = Some(Arc::clone(run));
                run_snapshot_changes.push(Self::run_snapshot_change(
                    &binding.resource.name,
                    &binding.resource.uid,
                    Some(previous),
                    Some(Arc::clone(run)),
                ));
            }
            for removed in Self::enforce_run_cap(runs, max_runs) {
                run_snapshot_changes.push(Self::run_snapshot_change(
                    &binding.resource.name,
                    &binding.resource.uid,
                    Some(removed),
                    None,
                ));
            }
        }
        self.transition_task_finished_locked(&mut inner, &binding, phase, error, exit_code, force);
        Self::record_run_snapshot_changes(&mut inner, run_snapshot_changes);
        if let Some(run) = run_change.as_ref() {
            self.record_run_change(
                &inner,
                &binding.resource.name,
                &binding.resource.uid,
                run.as_ref(),
            );
        }
        let name = binding.resource.name;
        Some(name)
    }

    /// Lists retained runs for a task.
    ///
    /// Results are ordered by generation and attempt.
    /// Unknown or swept history returns an empty list.
    #[cfg(test)]
    pub(crate) fn list_runs(&self, id: &TaskId) -> Vec<TaskRun> {
        let inner = self.inner.read();
        let mut runs: Vec<TaskRun> = inner
            .runs
            .get(id)
            .map(|runs| runs.iter().map(|run| run.as_ref().clone()).collect())
            .unwrap_or_default();
        runs.sort_by_key(|run| (run.generation(), run.attempt()));
        runs
    }

    /// Queries one task's runs with snapshot-consistent pagination.
    ///
    /// The first page returns `None` when the current task is absent.
    /// A continuation remains bound to the original Task name and UID.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub fn query_runs(
        &self,
        id: &TaskId,
        query: &TaskRunQuery,
    ) -> Result<Option<TaskRunPage>, CollectionError> {
        self.query_runs_where(id, query, |_| true)
    }

    /// Queries one task's runs through a caller predicate.
    ///
    /// The predicate runs before pagination and must stay stable across one
    /// continuation chain. A first-page query returns `None` when the current
    /// task is absent. Continuations reconstruct the original UID snapshot
    /// without requiring a current task.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub fn query_runs_where<F>(
        &self,
        id: &TaskId,
        query: &TaskRunQuery,
        predicate: F,
    ) -> Result<Option<TaskRunPage>, CollectionError>
    where
        F: Fn(&TaskRun) -> bool,
    {
        self.query_runs_where_visible(id, query, |_| true, predicate)
    }

    /// Queries runs with separate current-task and historical-run predicates.
    ///
    /// The current-task predicate is evaluated only for a first page.
    /// Both predicates run after the state lock is released.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub(crate) fn query_runs_where_visible<F, G>(
        &self,
        id: &TaskId,
        query: &TaskRunQuery,
        task_predicate: F,
        run_predicate: G,
    ) -> Result<Option<TaskRunPage>, CollectionError>
    where
        F: Fn(&Task) -> bool,
        G: Fn(&TaskRun) -> bool,
    {
        let inner = self.inner.read();
        let continuation = query.continuation();
        if let Some(cursor) = continuation
            && cursor.task() != id
        {
            return Err(CollectionError::TaskRunContinuationTaskMismatch {
                task: id.clone(),
                continuation_task: cursor.task().clone(),
            });
        }

        let (task_uid, resource_version, mut candidates, current_task) = match continuation {
            Some(cursor) => (
                cursor.task_uid().clone(),
                cursor.resource_version().to_owned(),
                Self::run_snapshot_at_resource_version(
                    &inner,
                    cursor.resource_version(),
                    cursor.task(),
                    cursor.task_uid(),
                )?
                .into_values()
                .collect::<Vec<_>>(),
                None,
            ),
            None => {
                let Some(task) = inner.tasks.get(id) else {
                    return Ok(None);
                };
                let runs = inner
                    .runs
                    .get(id)
                    .map(|runs| runs.iter().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                (
                    task.uid().clone(),
                    Self::current_run_resource_version(&inner),
                    runs,
                    Some(Arc::clone(task)),
                )
            }
        };
        drop(inner);
        candidates.sort_by_key(|run| (run.generation(), run.attempt()));
        if current_task.is_some_and(|task| !task_predicate(&task)) {
            return Ok(None);
        }

        let after = continuation.map(|cursor| (cursor.after_generation(), cursor.after_attempt()));
        let mut cursor_seen = after.is_none();
        let mut items = Vec::with_capacity(query.limit().min(candidates.len()));
        let mut item_bytes = 0usize;
        let mut page_closed = false;
        let mut remaining_item_count = 0usize;

        for run in candidates {
            if !run_predicate(run.as_ref()) {
                continue;
            }
            let key = (run.generation(), run.attempt());
            if let Some(after) = after
                && !cursor_seen
            {
                match key.cmp(&after) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => {
                        cursor_seen = true;
                        continue;
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(CollectionError::TaskRunContinuationCursorNotFound {
                            task: id.clone(),
                            generation: after.0,
                            attempt: after.1,
                        });
                    }
                }
            }
            if page_closed || items.len() >= query.limit() {
                remaining_item_count = remaining_item_count.saturating_add(1);
                continue;
            }

            let limit = query.item_byte_limit();
            let run_bytes = Self::serialized_run_payload_bytes(run.as_ref());
            if run_bytes > limit.get() && items.is_empty() {
                page_closed = true;
                items.push(run.as_ref().clone());
                continue;
            }
            let separator_bytes = usize::from(!items.is_empty());
            let projected = item_bytes
                .saturating_add(separator_bytes)
                .saturating_add(run_bytes);
            if projected > limit.get() {
                page_closed = true;
                remaining_item_count = remaining_item_count.saturating_add(1);
                continue;
            }
            item_bytes = projected;
            items.push(run.as_ref().clone());
        }

        if !cursor_seen {
            let (generation, attempt) =
                after.expect("a missing TaskRun cursor is seen before iteration");
            return Err(CollectionError::TaskRunContinuationCursorNotFound {
                task: id.clone(),
                generation,
                attempt,
            });
        }

        let next = if remaining_item_count > 0 {
            let after = items
                .last()
                .expect("positive TaskRun page limits return an item before remaining items");
            Some(
                TaskRunContinuation::new(
                    resource_version.clone(),
                    id.clone(),
                    task_uid.clone(),
                    after.generation(),
                    after.attempt(),
                )
                .expect("state-generated TaskRun continuations are valid"),
            )
        } else {
            None
        };

        Ok(Some(TaskRunPage {
            items,
            task: id.clone(),
            task_uid,
            resource_version,
            continuation: next,
            remaining_item_count,
        }))
    }

    /// Reconstructs one task UID's runs at a retained run revision.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] when the version is malformed, foreign,
    /// ahead of this store, or compacted.
    fn run_snapshot_at_resource_version(
        inner: &TaskStateInner,
        resource_version: &str,
        task: &TaskId,
        task_uid: &Uid,
    ) -> Result<BTreeMap<(u64, u32), Arc<TaskRun>>, CollectionError> {
        let (requested_epoch, requested_revision) = Self::parse_resource_version(resource_version)?;
        if requested_epoch != inner.run_resource_version_epoch {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: resource_version.to_owned(),
            });
        }
        if requested_revision > inner.run_resource_version {
            return Err(CollectionError::InvalidResourceVersion {
                resource_version: resource_version.to_owned(),
            });
        }
        if requested_revision < inner.run_compacted_through {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: resource_version.to_owned(),
            });
        }

        let mut snapshot = BTreeMap::new();
        if inner
            .tasks
            .get(task)
            .is_some_and(|current| current.uid() == task_uid)
            && let Some(runs) = inner.runs.get(task)
        {
            for run in runs {
                snapshot.insert((run.generation(), run.attempt()), run.clone());
            }
        }

        for batch in inner
            .run_history
            .iter()
            .rev()
            .take_while(|batch| batch.revision > requested_revision)
        {
            for change in batch
                .changes
                .iter()
                .rev()
                .filter(|change| &change.task == task && &change.task_uid == task_uid)
            {
                if let Some(previous) = change.previous.as_ref() {
                    snapshot.insert(
                        (previous.generation(), previous.attempt()),
                        previous.clone(),
                    );
                } else if let Some(current) = change.current.as_ref() {
                    snapshot.remove(&(current.generation(), current.attempt()));
                }
            }
        }
        Ok(snapshot)
    }

    /// Returns one retained task by name.
    pub fn get(&self, id: &TaskId) -> Option<Task> {
        self.get_retained(id)
    }

    /// Returns one retained task for internal use.
    pub(crate) fn get_retained(&self, id: &TaskId) -> Option<Task> {
        let inner = self.inner.read();
        inner.tasks.get(id).map(|task| task.as_ref().clone())
    }

    /// Returns whether a task exists.
    ///
    /// This is cheaper than [`get`](Self::get).
    /// It does not clone the task.
    pub fn contains_task(&self, id: &TaskId) -> bool {
        self.inner.read().tasks.contains_key(id)
    }

    /// Lists tasks in one slot.
    pub fn list_by_slot(&self, slot: &str) -> Vec<Task> {
        let inner = self.inner.read();

        inner
            .by_slot
            .get(slot)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| inner.tasks.get(id))
                    .map(|task| task.as_ref().clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Lists all retained tasks.
    pub fn list_all(&self) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .map(|task| task.as_ref().clone())
            .collect()
    }

    /// Lists tasks in one phase.
    pub fn list_by_status(&self, phase: TaskPhase) -> Vec<Task> {
        let inner = self.inner.read();
        inner
            .tasks
            .values()
            .filter(|task| task.status().phase() == phase)
            .map(|task| task.as_ref().clone())
            .collect()
    }

    /// Counts tasks by phase.
    ///
    /// This uses one read-lock pass.
    /// It does not clone [`Task`] values.
    /// Empty phases are absent.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    ///
    /// let state = TaskState::new();
    /// assert!(state.count_by_phase().is_empty());
    /// ```
    pub fn count_by_phase(&self) -> HashMap<TaskPhase, usize> {
        let inner = self.inner.read();
        let mut counts: HashMap<TaskPhase, usize> = HashMap::new();
        for task in inner.tasks.values() {
            *counts.entry(task.status().phase()).or_insert(0) += 1;
        }
        counts
    }

    /// Runs one retention sweep.
    ///
    /// The sweep removes expired terminal runs.
    /// It also removes expired nonterminal runs without a binding.
    /// Bound nonterminal runs remain.
    ///
    /// A terminal task is removed after its run history is empty and its task TTL expires.
    ///
    /// Returns `(runs_removed, tasks_removed)` for observability.
    pub(crate) fn sweep(&self, config: &StateConfig) -> (usize, usize) {
        let now = SystemTime::now();
        let (runs_removed, expired_tasks) = {
            let mut inner = self.write(StateMutationEventCapacity::None);
            let mut runs_removed = 0usize;
            let bound: std::collections::HashSet<TaskId> = inner.tv_of.keys().cloned().collect();
            let task_uids = inner
                .tasks
                .iter()
                .map(|(id, task)| (id.clone(), task.uid().clone()))
                .collect::<HashMap<_, _>>();
            let mut run_snapshot_changes = Vec::new();
            for (id, runs) in inner.runs.iter_mut() {
                let before = runs.len();
                let task_bound = bound.contains(id);
                let task_uid = task_uids
                    .get(id)
                    .expect("retained TaskRun history must belong to a retained task");
                runs.retain(|run| {
                    let retained = match run.finished_at() {
                        Some(finished) => now
                            .duration_since(finished)
                            .map(|age| age < config.run_ttl())
                            .unwrap_or(true),
                        None => {
                            task_bound
                                || now
                                    .duration_since(run.started_at())
                                    .map(|age| age < config.run_ttl())
                                    .unwrap_or(true)
                        }
                    };
                    if !retained {
                        run_snapshot_changes.push(Self::run_snapshot_change(
                            id,
                            task_uid,
                            Some(run.clone()),
                            None,
                        ));
                    }
                    retained
                });
                runs_removed += before - runs.len();
            }
            inner.runs.retain(|_, runs| !runs.is_empty());
            Self::record_run_snapshot_changes(&mut inner, run_snapshot_changes);

            let expired_tasks = inner
                .tasks
                .iter()
                .filter(|(id, task)| {
                    !inner.tv_of.contains_key(*id)
                        && task.status().phase().is_terminal()
                        && inner.runs.get(*id).is_none_or(|runs| runs.is_empty())
                        && inner.terminal_since.get(*id).is_some_and(|finished| {
                            now.duration_since(*finished)
                                .map(|age| age >= config.task_ttl())
                                .unwrap_or(false)
                        })
                })
                .map(|(id, task)| (id.clone(), task.uid().clone()))
                .collect::<Vec<_>>();
            (runs_removed, expired_tasks)
        };

        let mut tasks_removed = 0usize;
        for (id, expected_uid) in expired_tasks {
            let mut inner = self.write(StateMutationEventCapacity::TaskChange);
            let remains_expired = inner.tasks.get(&id).is_some_and(|task| {
                task.uid() == &expected_uid
                    && !inner.tv_of.contains_key(&id)
                    && task.status().phase().is_terminal()
                    && inner.runs.get(&id).is_none_or(|runs| runs.is_empty())
                    && inner.terminal_since.get(&id).is_some_and(|finished| {
                        now.duration_since(*finished)
                            .map(|age| age >= config.task_ttl())
                            .unwrap_or(false)
                    })
            });
            if !remains_expired {
                continue;
            }
            Self::unbind_locked(&mut inner, &id);
            if let Some(task) = inner.tasks.remove(&id) {
                Self::unindex_task(&mut inner, &task);
                Self::remove_retained_task_manifest_bytes(&mut inner, &id);
                inner.terminal_since.remove(&id);
                let (revision, _) = Self::next_resource_version(&mut inner);
                self.record_change(&mut inner, revision, Some(task), None);
                tasks_removed += 1;
            }
        }
        if runs_removed > 0 || tasks_removed > 0 {
            debug!(
                event = "state.sweep",
                runs_removed, tasks_removed, "state sweep completed"
            );
        }

        (runs_removed, tasks_removed)
    }

    /// Queries tasks with snapshot-consistent pagination.
    ///
    /// The first page captures the collection resource version.
    /// A continuation reads the same retained snapshot.
    ///
    /// Embedded tasks are visible.
    /// Transport filtering belongs to an adapter.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_core::TaskState;
    /// use solti_model::{TaskPhase, TaskQuery};
    ///
    /// let state = TaskState::new();
    /// let query = TaskQuery::new().with_phase(TaskPhase::Running).with_limit(10);
    /// let page = state.query(&query).unwrap();
    ///
    /// assert!(page.items.is_empty());
    /// assert_eq!(page.remaining_item_count, 0);
    /// ```
    pub fn query(&self, q: &TaskQuery) -> Result<TaskPage<Task>, CollectionError> {
        self.query_where(q, |_| true)
    }

    /// Queries tasks through a caller predicate.
    ///
    /// The predicate runs before pagination.
    /// It must stay stable across one continuation chain.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] for an invalid or unavailable continuation snapshot.
    pub fn query_where<F>(
        &self,
        q: &TaskQuery,
        predicate: F,
    ) -> Result<TaskPage<Task>, CollectionError>
    where
        F: Fn(&Task) -> bool,
    {
        let inner = self.inner.read();
        let continuation = q.continuation();
        if continuation.is_some_and(|cursor| cursor.filter() != q.filter()) {
            return Err(CollectionError::ContinuationFilterMismatch);
        }

        let (resource_version, candidates): (String, Vec<Arc<Task>>) = match continuation {
            Some(cursor) => (
                cursor.resource_version().to_owned(),
                Self::snapshot_at_resource_version(&inner, cursor.resource_version())?
                    .into_values()
                    .filter(|task| q.matches(task))
                    .collect::<Vec<_>>(),
            ),
            None => (
                Self::current_resource_version(&inner),
                match q.slot() {
                    Some(slot) => inner
                        .by_slot
                        .get(slot.as_str())
                        .into_iter()
                        .flatten()
                        .filter_map(|name| inner.tasks.get(name))
                        .filter(|task| q.matches(task))
                        .cloned()
                        .collect(),
                    None => inner
                        .ordered_tasks
                        .iter()
                        .filter_map(|name| inner.tasks.get(name))
                        .filter(|task| q.matches(task))
                        .cloned()
                        .collect(),
                },
            ),
        };
        drop(inner);

        let after = continuation.map(TaskContinuation::after);
        let mut cursor_seen = after.is_none();
        let mut items = Vec::with_capacity(q.limit().min(candidates.len()));
        let mut item_bytes = 0usize;
        let mut page_closed = false;
        let mut remaining_item_count = 0usize;

        for task in candidates {
            if !predicate(&task) {
                continue;
            }
            if let Some(after) = after
                && !cursor_seen
            {
                match task.name().cmp(after) {
                    std::cmp::Ordering::Less => continue,
                    std::cmp::Ordering::Equal => {
                        cursor_seen = true;
                        continue;
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(CollectionError::ContinuationCursorNotFound {
                            name: after.clone(),
                        });
                    }
                }
            }
            if page_closed || items.len() >= q.limit() {
                remaining_item_count = remaining_item_count.saturating_add(1);
                continue;
            }

            let limit = q.item_byte_limit();
            let task_bytes = Self::serialized_task_payload_bytes(None, Some(&task));
            if task_bytes > limit.get() && items.is_empty() {
                page_closed = true;
                items.push(task.as_ref().clone());
                continue;
            }
            let separator_bytes = usize::from(!items.is_empty());
            let projected = item_bytes
                .saturating_add(separator_bytes)
                .saturating_add(task_bytes);
            if projected > limit.get() {
                page_closed = true;
                remaining_item_count = remaining_item_count.saturating_add(1);
                continue;
            }
            item_bytes = projected;

            items.push(task.as_ref().clone());
        }
        if !cursor_seen {
            return Err(CollectionError::ContinuationCursorNotFound {
                name: after
                    .expect("a missing cursor is seen before iteration")
                    .clone(),
            });
        }
        let continuation = if remaining_item_count > 0 {
            let after = items
                .last()
                .expect("positive page limit with remaining items returns an item")
                .name()
                .clone();
            Some(
                TaskContinuation::new(resource_version.clone(), q.filter().clone(), after)
                    .expect("a state-generated resource version is not empty"),
            )
        } else {
            None
        };

        Ok(TaskPage {
            items,
            resource_version,
            continuation,
            remaining_item_count,
        })
    }

    fn snapshot_at_resource_version(
        inner: &TaskStateInner,
        resource_version: &str,
    ) -> Result<BTreeMap<TaskId, Arc<Task>>, CollectionError> {
        let (requested_epoch, requested_revision) = Self::parse_resource_version(resource_version)?;
        if requested_epoch != inner.resource_version_epoch.as_ref() {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: resource_version.to_owned(),
            });
        }
        if requested_revision > inner.resource_version {
            return Err(CollectionError::InvalidResourceVersion {
                resource_version: resource_version.to_owned(),
            });
        }
        if requested_revision < inner.compacted_through {
            return Err(CollectionError::ResourceVersionExpired {
                resource_version: resource_version.to_owned(),
            });
        }

        let mut snapshot = inner
            .ordered_tasks
            .iter()
            .filter_map(|name| {
                inner
                    .tasks
                    .get(name)
                    .map(|task| (name.clone(), Arc::clone(task)))
            })
            .collect::<BTreeMap<_, _>>();
        for change in inner
            .watch_history
            .iter()
            .rev()
            .take_while(|change| change.revision > requested_revision)
        {
            match (&change.previous, &change.current) {
                (Some(previous), _) => {
                    snapshot.insert(previous.name().clone(), Arc::clone(previous));
                }
                (None, Some(current)) => {
                    snapshot.remove(current.name());
                }
                (None, None) => {}
            }
        }
        Ok(snapshot)
    }

    /// Watches tasks selected by a filter.
    ///
    /// No version or `"0"` emits the current sorted snapshot first.
    /// Snapshot items are [`TaskWatchEvent::Added`].
    /// An exact version replays later retained changes before live changes.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] when the supplied resource version cannot be
    /// resumed or Task watch admission is full.
    pub fn watch(
        &self,
        filter: &TaskFilter,
        resource_version: Option<&str>,
    ) -> Result<TaskWatchSubscription, CollectionError> {
        self.watch_where(filter, resource_version, |_| true)
    }

    /// Watches tasks through a caller predicate.
    ///
    /// The predicate participates in transition classification.
    /// Entering visibility is `Added`.
    /// Leaving visibility is `Deleted`.
    ///
    /// # Errors
    ///
    /// Returns [`CollectionError`] when the supplied resource version cannot be
    /// resumed or Task watch admission is full.
    pub fn watch_where<F>(
        &self,
        filter: &TaskFilter,
        resource_version: Option<&str>,
        predicate: F,
    ) -> Result<TaskWatchSubscription, CollectionError>
    where
        F: Fn(&Task) -> bool + Send + Sync + 'static,
    {
        let predicate: WatchPredicate = Arc::new(predicate);
        let inner = self.inner.read();
        let receiver = BroadcastStream::new(inner.watch_tx.subscribe());
        let epoch = inner.resource_version_epoch.clone();
        let mut initial_candidates = None;
        let mut initial_revision = None;
        let mut replay_candidates = Vec::new();
        let last_revision;

        match resource_version {
            None | Some("0") => {
                initial_candidates = Some(
                    inner
                        .ordered_tasks
                        .iter()
                        .filter_map(|name| inner.tasks.get(name))
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                initial_revision = Some(inner.resource_version);
                last_revision = 0;
            }
            Some(value) => {
                let (requested_epoch, requested_revision) = Self::parse_resource_version(value)?;
                if requested_epoch != epoch.as_ref() {
                    return Err(CollectionError::ResourceVersionExpired {
                        resource_version: value.to_string(),
                    });
                }
                if requested_revision > inner.resource_version {
                    return Err(CollectionError::InvalidResourceVersion {
                        resource_version: value.to_string(),
                    });
                }
                if requested_revision < inner.compacted_through {
                    return Err(CollectionError::ResourceVersionExpired {
                        resource_version: value.to_string(),
                    });
                }
                replay_candidates.extend(
                    inner
                        .watch_history
                        .iter()
                        .filter(|change| change.revision > requested_revision)
                        .cloned(),
                );
                last_revision = requested_revision;
            }
        }
        drop(inner);

        match self.watch_admission.precheck_count() {
            Ok(()) => {}
            Err(WatchAdmissionFailure::Rejected(error)) => return Err(error),
            Err(WatchAdmissionFailure::Closed) => {
                return Ok(TaskWatchSubscription {
                    inner: Arc::clone(&self.inner),
                    receiver,
                    initial: VecDeque::new(),
                    initial_revision: None,
                    replay: VecDeque::new(),
                    recovery: None,
                    permit: None,
                    filter: filter.clone(),
                    predicate,
                    epoch,
                    last_revision,
                    stop: Box::pin(self.watch_stop.clone().cancelled_owned()),
                    terminal: false,
                });
            }
        }

        let initial_descriptors = initial_candidates
            .unwrap_or_default()
            .into_iter()
            .filter(|task| filter.matches(task) && predicate(task))
            .map(|task| WatchEventDescriptor {
                kind: PreparedWatchEventKind::Added,
                serialized_bytes: Self::serialized_task_payload_bytes(None, Some(task.as_ref())),
                task,
                resource_version: None,
            })
            .collect::<Vec<_>>();
        let replay_descriptors = replay_candidates
            .into_iter()
            .map(|change| {
                let revision = change.revision;
                let event = {
                    let previous_matches = change
                        .previous
                        .as_ref()
                        .is_some_and(|task| filter.matches(task) && predicate(task));
                    let current_matches = change
                        .current
                        .as_ref()
                        .is_some_and(|task| filter.matches(task) && predicate(task));
                    match (previous_matches, current_matches) {
                        (false, true) => change.current.as_ref().map(|task| WatchEventDescriptor {
                            kind: PreparedWatchEventKind::Added,
                            serialized_bytes: Self::serialized_task_payload_bytes(
                                None,
                                Some(task.as_ref()),
                            ),
                            task: Arc::clone(task),
                            resource_version: None,
                        }),
                        (true, true) => change.current.as_ref().map(|task| WatchEventDescriptor {
                            kind: PreparedWatchEventKind::Modified,
                            serialized_bytes: Self::serialized_task_payload_bytes(
                                None,
                                Some(task.as_ref()),
                            ),
                            task: Arc::clone(task),
                            resource_version: None,
                        }),
                        (true, false) => change.previous.as_ref().map(|task| {
                            let resource_version = Self::format_resource_version(
                                change.epoch.as_ref(),
                                change.revision,
                            );
                            WatchEventDescriptor {
                                kind: PreparedWatchEventKind::Deleted,
                                serialized_bytes: Self::serialized_task_with_resource_version_bytes(
                                    task.as_ref(),
                                    &resource_version,
                                ),
                                task: Arc::clone(task),
                                resource_version: Some(resource_version),
                            }
                        }),
                        (false, false) => None,
                    }
                };
                (revision, event)
            })
            .collect::<Vec<_>>();
        let requested_bytes = initial_descriptors
            .iter()
            .map(|event| event.serialized_bytes)
            .chain(
                replay_descriptors
                    .iter()
                    .filter_map(|(_, event)| event.as_ref().map(|event| event.serialized_bytes)),
            )
            .fold(0_usize, usize::saturating_add);
        let permit = match self.watch_admission.try_admit(requested_bytes) {
            Ok(permit) => permit,
            Err(WatchAdmissionFailure::Rejected(error)) => return Err(error),
            Err(WatchAdmissionFailure::Closed) => {
                return Ok(TaskWatchSubscription {
                    inner: Arc::clone(&self.inner),
                    receiver,
                    initial: VecDeque::new(),
                    initial_revision: None,
                    replay: VecDeque::new(),
                    recovery: None,
                    permit: None,
                    filter: filter.clone(),
                    predicate,
                    epoch,
                    last_revision,
                    stop: Box::pin(self.watch_stop.clone().cancelled_owned()),
                    terminal: false,
                });
            }
        };
        let initial = initial_descriptors
            .into_iter()
            .map(WatchEventDescriptor::into_buffered)
            .collect();
        let replay = replay_descriptors
            .into_iter()
            .map(|(revision, event)| PreparedReplayChange {
                revision,
                event: event.map(WatchEventDescriptor::into_buffered),
            })
            .collect();

        if !permit.is_active() || self.watch_stop.is_cancelled() {
            return Ok(TaskWatchSubscription {
                inner: Arc::clone(&self.inner),
                receiver,
                initial: VecDeque::new(),
                initial_revision: None,
                replay: VecDeque::new(),
                recovery: None,
                permit: None,
                filter: filter.clone(),
                predicate,
                epoch,
                last_revision,
                stop: Box::pin(self.watch_stop.clone().cancelled_owned()),
                terminal: false,
            });
        }

        Ok(TaskWatchSubscription {
            inner: Arc::clone(&self.inner),
            receiver,
            initial,
            initial_revision,
            replay,
            recovery: None,
            permit: Some(permit),
            filter: filter.clone(),
            predicate,
            epoch,
            last_revision,
            stop: Box::pin(self.watch_stop.clone().cancelled_owned()),
            terminal: false,
        })
    }

    fn parse_resource_version(value: &str) -> Result<(&str, u64), CollectionError> {
        let invalid = || CollectionError::InvalidResourceVersion {
            resource_version: value.to_string(),
        };
        if value.is_empty() {
            return Err(invalid());
        }
        let (epoch, revision) = value.split_once(':').ok_or_else(&invalid)?;
        if epoch.is_empty() || revision.is_empty() || revision.contains(':') {
            return Err(invalid());
        }
        let revision = revision.parse::<u64>().map_err(|_| invalid())?;
        Ok((epoch, revision))
    }

    pub(crate) fn close_watches(&self) {
        self.watch_stop.cancel();
        self.watch_admission.close();
    }

    pub(crate) async fn shutdown_persistence(&self) {
        self.event_publisher.shutdown().await;
    }

    pub(crate) fn persistence_status(&self) -> Option<TaskStateSinkStatus> {
        self.event_publisher.status()
    }
}

impl Default for TaskState {
    fn default() -> Self {
        Self::new()
    }
}

/// Test fixtures for direct state population.
#[cfg(feature = "test-util")]
impl TaskState {
    /// Seeds a task directly.
    pub fn seed_task(&self, id: TaskId, spec: solti_model::TaskSpec) {
        self.add_task(TaskManifest::new(id, spec).expect("test fixture must be valid"));
    }

    /// Moves a seeded task to `Running`.
    ///
    /// # Panics
    ///
    /// Panics when the task is missing or cannot start.
    pub fn seed_starting(&self, id: &TaskId) {
        let task = self.get(id).expect("seeded task must exist");
        let resource = ResourceGeneration::from_task(&task);
        let tv = taskvisor::TaskId::for_tests();
        assert!(self.bind_tv(resource.clone(), tv));
        let binding = RuntimeBinding { resource, tv };
        assert!(self.transition_attempt_starting(&binding, 1));
    }

    /// Moves a seeded task to a terminal phase.
    ///
    /// # Panics
    ///
    /// Panics when the task is missing or the transition is invalid.
    pub fn seed_finished(
        &self,
        id: &TaskId,
        phase: TaskPhase,
        error: Option<String>,
        exit_code: Option<i32>,
    ) {
        let binding = self.binding_for(id).unwrap_or_else(|| {
            let task = self.get(id).expect("seeded task must exist");
            let resource = ResourceGeneration::from_task(&task);
            let tv = taskvisor::TaskId::for_tests();
            assert!(self.bind_tv(resource.clone(), tv));
            RuntimeBinding { resource, tv }
        });
        let attempt = self
            .get(id)
            .map(|task| task.status().attempt().max(1))
            .unwrap_or(1);
        assert!(self.transition_attempt_finished(&binding, attempt, phase, error, exit_code));
    }
}

#[cfg(test)]
mod tests;
