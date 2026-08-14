//! # Persistence hooks
//!
//! These hooks let an agent forward task state and output to external storage.
//!
//! State events enter a bounded, lossless core-owned queue after the authoritative
//! state lock is released. Queue saturation applies backpressure to the commit.
//! One worker delivers callbacks in commit order and shutdown drains that queue.
//! Output callbacks use a separate bounded, nonblocking, best-effort dispatcher.

use std::{
    cell::Cell,
    collections::VecDeque,
    io,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use parking_lot::{Condvar, Mutex};
use solti_model::{OutputEvent, Task, TaskId, TaskRun, Uid};

use crate::ConfigError;

const DEFAULT_STATE_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2_048).unwrap();
const DEFAULT_OUTPUT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2_048).unwrap();
pub(crate) const MAX_STATE_EVENTS_PER_COMMIT: usize = 3;
const MIN_STATE_QUEUE_CAPACITY: usize = MAX_STATE_EVENTS_PER_COMMIT - 1;
const MAX_STATE_QUEUE_CAPACITY: usize = usize::MAX - 1;

/// Persistence delivery settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistenceConfig {
    state_queue_capacity: NonZeroUsize,
    output_queue_capacity: NonZeroUsize,
}

impl PersistenceConfig {
    /// Creates the default bounded persistence settings.
    pub const fn new() -> Self {
        Self {
            state_queue_capacity: DEFAULT_STATE_QUEUE_CAPACITY,
            output_queue_capacity: DEFAULT_OUTPUT_QUEUE_CAPACITY,
        }
    }

    /// Returns `C` for the hard admission bound `reserved + buffered + active <= C + 1`.
    /// The active callback count is either zero or one.
    pub const fn state_queue_capacity(self) -> NonZeroUsize {
        self.state_queue_capacity
    }

    /// Returns the hard output-event admission bound, including the active callback.
    pub const fn output_queue_capacity(self) -> NonZeroUsize {
        self.output_queue_capacity
    }

    /// Replaces the committed state-event capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `capacity` is zero.
    /// Returns [`ConfigError::BelowMinimum`] when `capacity` cannot admit the
    /// largest atomic state commit alongside the active callback.
    /// Returns [`ConfigError::Exceeds`] when adding the active callback slot
    /// would overflow the platform's capacity type.
    pub const fn try_with_state_queue_capacity(
        mut self,
        capacity: usize,
    ) -> Result<Self, ConfigError> {
        let Some(capacity) = NonZeroUsize::new(capacity) else {
            return Err(ConfigError::Zero {
                field: "persistence_state_queue_capacity",
            });
        };
        if capacity.get() < MIN_STATE_QUEUE_CAPACITY {
            return Err(ConfigError::BelowMinimum {
                field: "persistence_state_queue_capacity",
                minimum: MIN_STATE_QUEUE_CAPACITY,
            });
        }
        if capacity.get() > MAX_STATE_QUEUE_CAPACITY {
            return Err(ConfigError::Exceeds {
                field: "persistence_state_queue_capacity",
                limit: "persistence_state_queue_capacity_max",
            });
        }
        self.state_queue_capacity = capacity;
        Ok(self)
    }

    /// Replaces the hard output-event admission bound.
    ///
    /// The bound counts buffered events and the event in the active callback.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `capacity` is zero.
    pub const fn try_with_output_queue_capacity(
        mut self,
        capacity: usize,
    ) -> Result<Self, ConfigError> {
        let Some(capacity) = NonZeroUsize::new(capacity) else {
            return Err(ConfigError::Zero {
                field: "persistence_output_queue_capacity",
            });
        };
        self.output_queue_capacity = capacity;
        Ok(self)
    }
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// One committed change to task or run state.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TaskStateEvent {
    /// One task resource was created, changed, or deleted.
    ///
    /// Create has no previous value.
    /// Delete has no current value.
    /// Task snapshots use [`Arc`] so cloned events remain cheap to forward.
    TaskChanged {
        /// Resource version assigned to this change.
        resource_version: String,
        /// Resource value before the change.
        previous: Option<Arc<Task>>,
        /// Resource value after the change.
        current: Option<Arc<Task>>,
    },
    /// One run was created or changed.
    ///
    /// Run retention does not publish delete events.
    /// This hook is a lifecycle journal, not a mirror of the in-memory retention window.
    RunChanged {
        /// Stable task name that owns the run.
        task: TaskId,
        /// Exact task incarnation that owns the run.
        task_uid: Uid,
        /// Current run value after the change.
        run: TaskRun,
    },
}

/// Receiver of committed task state changes.
///
/// Core invokes this callback on one dedicated persistence worker. Calls are
/// serialized in commit order. A slow sink fills the bounded queue and then
/// applies backpressure to state commits; events are not dropped for overload.
/// The callback must eventually return so shutdown can drain the queue.
/// It must not mutate `TaskState`, directly or by waiting for another thread
/// that does so. Reads are allowed. Polling [`crate::SupervisorApi::shutdown`]
/// on the callback worker panics before shutdown starts. Waiting for another
/// thread that calls shutdown can deadlock and is also forbidden.
pub trait TaskStateSink: Send + Sync + 'static {
    /// Receives one committed state event.
    fn on_event(&self, event: &TaskStateEvent);
}

/// Shared task state sink.
pub type TaskStateSinkHandle = Arc<dyn TaskStateSink>;

/// Observable state of the task-state persistence worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskStateSinkStatus {
    accepting: bool,
    healthy: bool,
    queued: usize,
    capacity: usize,
    delivered: u64,
    failed: u64,
}

impl TaskStateSinkStatus {
    /// Returns whether the dispatcher accepts reservations for new events.
    pub fn accepting(self) -> bool {
        self.accepting
    }

    /// Returns whether no callback or worker panic has been observed.
    ///
    /// Health is sticky. One callback or worker panic keeps it false.
    pub fn healthy(self) -> bool {
        self.healthy
    }

    /// Returns reserved, buffered, and actively delivered event ownership.
    pub fn queued(self) -> usize {
        self.queued
    }

    /// Returns the hard event-ownership bound, including the active callback.
    pub fn capacity(self) -> usize {
        self.capacity
    }

    /// Returns callbacks that returned normally.
    ///
    /// The callback API cannot report application-level storage success.
    pub fn delivered(self) -> u64 {
        self.delivered
    }

    /// Returns callbacks that panicked.
    ///
    /// A panicking callback has ambiguous side effects and is not retried.
    pub fn failed(self) -> u64 {
        self.failed
    }
}

type StateEventBatch = Vec<StateDispatchEvent>;

pub(crate) struct StateDispatchEvent {
    event: TaskStateEvent,
    _permit: StateQueuePermit,
}

impl StateDispatchEvent {
    pub(crate) fn new(event: TaskStateEvent, permit: StateQueuePermit) -> Self {
        Self {
            event,
            _permit: permit,
        }
    }
}

struct StateQueuePermits {
    state: Mutex<StateQueuePermitState>,
    limit: usize,
    metrics: Arc<StateDispatcherMetrics>,
}

struct StateQueuePermitState {
    available: usize,
    waiters: VecDeque<Arc<StateQueueWaiter>>,
}

struct StateQueueWaiter {
    requested: usize,
    ready: Condvar,
}

pub(crate) struct StateQueuePermit {
    permits: Arc<StateQueuePermits>,
}

impl Drop for StateQueuePermit {
    fn drop(&mut self) {
        self.permits
            .metrics
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued.checked_sub(1)
            })
            .expect("state persistence outstanding count must not underflow");
        let mut state = self.permits.state.lock();
        state.available = state
            .available
            .checked_add(1)
            .expect("state persistence permit count must not overflow");
        assert!(
            state.available <= self.permits.limit,
            "state persistence permits must not be released more than once"
        );
        self.permits.notify_front_if_ready(&state);
    }
}

impl StateQueuePermits {
    fn reserve(self: &Arc<Self>, requested: usize) -> Vec<StateQueuePermit> {
        assert!(
            requested <= self.limit,
            "an atomic state commit must fit within persistence event capacity"
        );
        if requested == 0 {
            return Vec::new();
        }

        let mut state = self.state.lock();
        if state.waiters.is_empty() && state.available >= requested {
            state.available -= requested;
            drop(state);
            return self.make_permits(requested);
        }

        let waiter = Arc::new(StateQueueWaiter {
            requested,
            ready: Condvar::new(),
        });
        state.waiters.push_back(Arc::clone(&waiter));
        loop {
            let is_front = state
                .waiters
                .front()
                .is_some_and(|front| Arc::ptr_eq(front, &waiter));
            if is_front && state.available >= requested {
                state.available -= requested;
                let admitted = state
                    .waiters
                    .pop_front()
                    .expect("the admitted state persistence waiter is queued");
                debug_assert!(Arc::ptr_eq(&admitted, &waiter));
                self.notify_front_if_ready(&state);
                drop(state);
                return self.make_permits(requested);
            }
            waiter.ready.wait(&mut state);
        }
    }

    fn make_permits(self: &Arc<Self>, count: usize) -> Vec<StateQueuePermit> {
        self.metrics
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(count)
                    .filter(|updated| *updated <= self.limit)
            })
            .expect("state persistence outstanding count must remain bounded");
        (0..count)
            .map(|_| StateQueuePermit {
                permits: Arc::clone(self),
            })
            .collect()
    }

    fn notify_front_if_ready(&self, state: &StateQueuePermitState) {
        if let Some(waiter) = state.waiters.front()
            && state.available >= waiter.requested
        {
            waiter.ready.notify_one();
        }
    }
}

struct StateDispatcherMetrics {
    accepting: AtomicBool,
    healthy: AtomicBool,
    queued: AtomicUsize,
    capacity: usize,
    delivered: AtomicU64,
    failed: AtomicU64,
}

impl StateDispatcherMetrics {
    fn status(&self) -> TaskStateSinkStatus {
        TaskStateSinkStatus {
            accepting: self.accepting.load(Ordering::Acquire),
            healthy: self.healthy.load(Ordering::Acquire),
            queued: self.queued.load(Ordering::Acquire),
            capacity: self.capacity,
            delivered: self.delivered.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }

    fn mark_worker_failed(&self) {
        self.healthy.store(false, Ordering::Release);
        self.accepting.store(false, Ordering::Release);
    }
}

fn saturating_increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        Some(value.saturating_add(1))
    });
}

thread_local! {
    static IN_STATE_SINK_CALLBACK: Cell<bool> = const { Cell::new(false) };
    static IN_OUTPUT_SINK_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateDispatcherShutdownOutcome {
    Pending,
    Drained,
    WorkerPanicked,
}

struct StateDispatcherShutdown {
    started: AtomicBool,
    // `watch` retains the terminal result even when every current waiter is canceled.
    outcome: tokio::sync::watch::Sender<StateDispatcherShutdownOutcome>,
}

impl StateDispatcherShutdown {
    fn new() -> Self {
        let (outcome, _) = tokio::sync::watch::channel(StateDispatcherShutdownOutcome::Pending);
        Self {
            started: AtomicBool::new(false),
            outcome,
        }
    }

    fn begin(&self) -> bool {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    async fn wait(&self) {
        let mut outcome = self.outcome.subscribe();
        loop {
            match *outcome.borrow_and_update() {
                StateDispatcherShutdownOutcome::Pending => {}
                StateDispatcherShutdownOutcome::Drained => return,
                StateDispatcherShutdownOutcome::WorkerPanicked => {
                    panic!("state persistence worker must not panic");
                }
            }
            outcome
                .changed()
                .await
                .expect("state persistence shutdown completion remains available");
        }
    }
}

pub(crate) struct StateEventDispatcher {
    sender: Mutex<Option<mpsc::Sender<StateDispatchEvent>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    permits: Arc<StateQueuePermits>,
    metrics: Arc<StateDispatcherMetrics>,
    shutdown: StateDispatcherShutdown,
}

impl StateEventDispatcher {
    pub(crate) fn start(sink: TaskStateSinkHandle, capacity: NonZeroUsize) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<StateDispatchEvent>();
        // `capacity` counts buffered events. One additional permit belongs to
        // the callback currently executing on the persistence worker.
        let permit_limit = capacity
            .get()
            .checked_add(1)
            .expect("validated persistence capacity leaves one active event slot");
        let metrics = Arc::new(StateDispatcherMetrics {
            accepting: AtomicBool::new(true),
            healthy: AtomicBool::new(true),
            queued: AtomicUsize::new(0),
            capacity: permit_limit,
            delivered: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        });
        let worker_metrics = Arc::clone(&metrics);
        let worker = thread::Builder::new()
            .name("solti-state-persistence".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    while let Ok(dispatched) = receiver.recv() {
                        if deliver_state_event(&sink, dispatched.event) {
                            saturating_increment(&worker_metrics.delivered);
                        } else {
                            worker_metrics.healthy.store(false, Ordering::Release);
                            saturating_increment(&worker_metrics.failed);
                        }
                    }
                }));
                if let Err(payload) = result {
                    worker_metrics.mark_worker_failed();
                    std::panic::resume_unwind(payload);
                }
            })?;
        let permits = Arc::new(StateQueuePermits {
            state: Mutex::new(StateQueuePermitState {
                available: permit_limit,
                waiters: VecDeque::new(),
            }),
            limit: permit_limit,
            metrics: Arc::clone(&metrics),
        });
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            permits,
            metrics,
            shutdown: StateDispatcherShutdown::new(),
        })
    }

    pub(crate) fn reserve(&self, event_count: usize) -> Vec<StateQueuePermit> {
        assert_state_sink_is_not_mutating_state();
        self.permits.reserve(event_count)
    }

    pub(crate) fn dispatch(&self, events: StateEventBatch) {
        let sender = self
            .sender
            .lock()
            .as_ref()
            .cloned()
            .expect("state persistence dispatcher is open while commits are accepted");
        for event in events {
            if sender.send(event).is_err() {
                self.metrics.mark_worker_failed();
                panic!("state persistence worker must remain available");
            }
        }
    }

    pub(crate) fn status(&self) -> TaskStateSinkStatus {
        self.metrics.status()
    }

    pub(crate) async fn shutdown(&self) {
        if self.shutdown.begin() {
            self.metrics.accepting.store(false, Ordering::Release);
            self.sender.lock().take();
            let worker = self.worker.lock().take();
            let outcome = self.shutdown.outcome.clone();
            if let Some(worker) = worker {
                // The detached join owns completion publication. Canceling the
                // caller can only remove that caller's wait, never the drain.
                drop(tokio::task::spawn_blocking(move || {
                    let completed = if worker.join().is_ok() {
                        StateDispatcherShutdownOutcome::Drained
                    } else {
                        StateDispatcherShutdownOutcome::WorkerPanicked
                    };
                    outcome.send_replace(completed);
                }));
            } else {
                self.shutdown
                    .outcome
                    .send_replace(StateDispatcherShutdownOutcome::Drained);
            }
        }
        self.shutdown.wait().await;
    }
}

impl Drop for StateEventDispatcher {
    fn drop(&mut self) {
        self.metrics.accepting.store(false, Ordering::Release);
        self.sender.get_mut().take();
        if let Some(worker) = self.worker.get_mut().take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// One output event with its task name and exact resource UID.
#[derive(Clone, Debug)]
pub struct TaskOutputEvent {
    task: TaskId,
    task_uid: Uid,
    event: OutputEvent,
}

impl TaskOutputEvent {
    pub(crate) fn new(task: TaskId, task_uid: Uid, event: OutputEvent) -> Self {
        Self {
            task,
            task_uid,
            event,
        }
    }

    /// Returns the task name that produced the output.
    pub fn task(&self) -> &TaskId {
        &self.task
    }

    /// Returns the exact task incarnation that produced the output.
    pub fn task_uid(&self) -> &Uid {
        &self.task_uid
    }

    /// Returns the original output event.
    pub fn event(&self) -> &OutputEvent {
        &self.event
    }

    /// Splits the wrapper into its task name, UID, and output event.
    pub fn into_parts(self) -> (TaskId, Uid, OutputEvent) {
        (self.task, self.task_uid, self.event)
    }
}

/// Receiver of task output events.
///
/// Core invokes this callback on one dedicated output-persistence worker. The
/// sink receives output from the first event because it is installed before
/// runners start. Runner publication never waits for callback capacity. A full,
/// closed, contended, or unhealthy dispatcher drops only the callback copy.
/// A callback panic is not retried, marks health false, and closes new admission;
/// the worker drains events accepted before that panic. The callback must
/// eventually return so shutdown can drain accepted events. It must not poll
/// [`crate::SupervisorApi::shutdown`] on the callback worker; that panics before
/// shutdown starts. It must not wait for another thread that calls shutdown,
/// because that can deadlock the drain.
pub trait TaskOutputSink: Send + Sync + 'static {
    /// Receives one task output event.
    fn on_event(&self, event: &TaskOutputEvent);
}

/// Shared task output sink.
pub type TaskOutputSinkHandle = Arc<dyn TaskOutputSink>;

/// Observable state of the task-output persistence worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskOutputSinkStatus {
    accepting: bool,
    healthy: bool,
    queued: usize,
    capacity: usize,
    delivered: u64,
    failed: u64,
    dropped: u64,
}

impl TaskOutputSinkStatus {
    /// Returns whether the dispatcher accepts new callback copies.
    pub fn accepting(self) -> bool {
        self.accepting
    }

    /// Returns whether no callback or worker panic has been observed.
    ///
    /// Health is sticky. One callback or worker panic keeps it false.
    pub fn healthy(self) -> bool {
        self.healthy
    }

    /// Returns buffered and actively delivered event ownership.
    pub fn queued(self) -> usize {
        self.queued
    }

    /// Returns the hard event-ownership bound, including the active callback.
    pub fn capacity(self) -> usize {
        self.capacity
    }

    /// Returns callbacks that returned normally.
    ///
    /// The callback API cannot report application-level storage success.
    pub fn delivered(self) -> u64 {
        self.delivered
    }

    /// Returns callbacks that panicked.
    ///
    /// A panicking callback has ambiguous side effects and is not retried.
    pub fn failed(self) -> u64 {
        self.failed
    }

    /// Returns callback copies rejected by admission.
    ///
    /// A full, closed, contended, or unhealthy dispatcher rejects only the
    /// external callback copy. Live output and task execution continue.
    pub fn dropped(self) -> u64 {
        self.dropped
    }
}

struct OutputDispatcherAdmission {
    accepting: bool,
    sender: Option<mpsc::Sender<OutputDispatchEvent>>,
}

struct OutputDispatcherMetrics {
    admission: Mutex<OutputDispatcherAdmission>,
    accepting: AtomicBool,
    healthy: AtomicBool,
    queued: AtomicUsize,
    capacity: usize,
    delivered: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
    #[cfg(test)]
    panic_worker_once: AtomicBool,
}

impl OutputDispatcherMetrics {
    fn status(&self) -> TaskOutputSinkStatus {
        TaskOutputSinkStatus {
            accepting: self.accepting.load(Ordering::Acquire),
            healthy: self.healthy.load(Ordering::Acquire),
            queued: self.queued.load(Ordering::Acquire),
            capacity: self.capacity,
            delivered: self.delivered.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Acquire),
        }
    }

    fn close_admission(&self) {
        let mut admission = self.admission.lock();
        admission.accepting = false;
        self.accepting.store(false, Ordering::Release);
        admission.sender.take();
    }

    fn mark_callback_failed(&self) {
        saturating_increment(&self.failed);
        self.close_admission();
        self.healthy.store(false, Ordering::Release);
    }

    fn mark_worker_failed(&self) {
        self.close_admission();
        self.healthy.store(false, Ordering::Release);
    }

    fn drop_callback_copy(&self) {
        saturating_increment(&self.dropped);
    }
}

struct OutputQueuePermit {
    metrics: Arc<OutputDispatcherMetrics>,
}

impl Drop for OutputQueuePermit {
    fn drop(&mut self) {
        self.metrics
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued.checked_sub(1)
            })
            .expect("output persistence outstanding count must not underflow");
    }
}

struct OutputDispatchEvent {
    event: TaskOutputEvent,
    _permit: OutputQueuePermit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputDispatcherShutdownOutcome {
    Pending,
    Drained,
    WorkerPanicked,
}

struct OutputDispatcherShutdown {
    started: AtomicBool,
    // `watch` retains the terminal result even when every current waiter is canceled.
    outcome: tokio::sync::watch::Sender<OutputDispatcherShutdownOutcome>,
}

impl OutputDispatcherShutdown {
    fn new() -> Self {
        let (outcome, _) = tokio::sync::watch::channel(OutputDispatcherShutdownOutcome::Pending);
        Self {
            started: AtomicBool::new(false),
            outcome,
        }
    }

    fn begin(&self) -> bool {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    async fn wait(&self) {
        let mut outcome = self.outcome.subscribe();
        loop {
            match *outcome.borrow_and_update() {
                OutputDispatcherShutdownOutcome::Pending => {}
                OutputDispatcherShutdownOutcome::Drained => return,
                OutputDispatcherShutdownOutcome::WorkerPanicked => {
                    panic!("output persistence worker must not panic");
                }
            }
            outcome
                .changed()
                .await
                .expect("output persistence shutdown completion remains available");
        }
    }
}

pub(crate) struct OutputEventDispatcher {
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    metrics: Arc<OutputDispatcherMetrics>,
    shutdown: OutputDispatcherShutdown,
}

impl OutputEventDispatcher {
    pub(crate) fn start(sink: TaskOutputSinkHandle, capacity: NonZeroUsize) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<OutputDispatchEvent>();
        let metrics = Arc::new(OutputDispatcherMetrics {
            admission: Mutex::new(OutputDispatcherAdmission {
                accepting: true,
                sender: Some(sender),
            }),
            accepting: AtomicBool::new(true),
            healthy: AtomicBool::new(true),
            queued: AtomicUsize::new(0),
            capacity: capacity.get(),
            delivered: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            #[cfg(test)]
            panic_worker_once: AtomicBool::new(false),
        });
        let worker_metrics = Arc::clone(&metrics);
        let worker = thread::Builder::new()
            .name("solti-output-persistence".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    while let Ok(dispatched) = receiver.recv() {
                        #[cfg(test)]
                        if worker_metrics
                            .panic_worker_once
                            .swap(false, Ordering::AcqRel)
                        {
                            panic!("injected output persistence worker panic");
                        }
                        if deliver_output_event(&sink, &dispatched.event) {
                            saturating_increment(&worker_metrics.delivered);
                        } else {
                            worker_metrics.mark_callback_failed();
                        }
                    }
                }));
                if let Err(payload) = result {
                    worker_metrics.mark_worker_failed();
                    std::panic::resume_unwind(payload);
                }
            })?;
        Ok(Self {
            worker: Mutex::new(Some(worker)),
            metrics,
            shutdown: OutputDispatcherShutdown::new(),
        })
    }

    /// Attempts one callback-copy admission without waiting for capacity or locks.
    pub(crate) fn try_dispatch(&self, event: TaskOutputEvent) -> bool {
        if !self.metrics.healthy.load(Ordering::Acquire) {
            self.metrics.drop_callback_copy();
            return false;
        }
        let Some(mut admission) = self.metrics.admission.try_lock() else {
            self.metrics.drop_callback_copy();
            return false;
        };
        if !admission.accepting || !self.metrics.healthy.load(Ordering::Acquire) {
            self.metrics.drop_callback_copy();
            return false;
        }
        let admitted =
            self.metrics
                .queued
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    queued
                        .checked_add(1)
                        .filter(|updated| *updated <= self.metrics.capacity)
                });
        if admitted.is_err() {
            self.metrics.drop_callback_copy();
            return false;
        }
        let dispatched = OutputDispatchEvent {
            event,
            _permit: OutputQueuePermit {
                metrics: Arc::clone(&self.metrics),
            },
        };
        let sent = admission
            .sender
            .as_ref()
            .is_some_and(|sender| sender.send(dispatched).is_ok());
        if sent {
            true
        } else {
            admission.accepting = false;
            self.metrics.accepting.store(false, Ordering::Release);
            admission.sender.take();
            self.metrics.healthy.store(false, Ordering::Release);
            self.metrics.drop_callback_copy();
            false
        }
    }

    pub(crate) fn status(&self) -> TaskOutputSinkStatus {
        self.metrics.status()
    }

    pub(crate) async fn shutdown(&self) {
        if self.shutdown.begin() {
            self.metrics.close_admission();
            let worker = self.worker.lock().take();
            let outcome = self.shutdown.outcome.clone();
            if let Some(worker) = worker {
                // The detached join owns completion publication. Canceling the
                // caller can only remove that caller's wait, never the drain.
                drop(tokio::task::spawn_blocking(move || {
                    let completed = if worker.join().is_ok() {
                        OutputDispatcherShutdownOutcome::Drained
                    } else {
                        OutputDispatcherShutdownOutcome::WorkerPanicked
                    };
                    outcome.send_replace(completed);
                }));
            } else {
                self.shutdown
                    .outcome
                    .send_replace(OutputDispatcherShutdownOutcome::Drained);
            }
        }
        self.shutdown.wait().await;
    }

    #[cfg(test)]
    fn inject_worker_panic(&self) {
        self.metrics
            .panic_worker_once
            .store(true, Ordering::Release);
    }
}

impl Drop for OutputEventDispatcher {
    fn drop(&mut self) {
        self.metrics.close_admission();
        if let Some(worker) = self.worker.get_mut().take()
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

pub(crate) struct PersistenceSinks {
    pub(crate) state: Option<TaskStateSinkHandle>,
    pub(crate) output: Option<TaskOutputSinkHandle>,
    pub(crate) config: PersistenceConfig,
}

fn deliver_state_event(sink: &TaskStateSinkHandle, event: TaskStateEvent) -> bool {
    let result = IN_STATE_SINK_CALLBACK.with(|active| {
        let was_active = active.replace(true);
        debug_assert!(!was_active);
        let result = catch_unwind(AssertUnwindSafe(|| sink.on_event(&event)));
        active.set(false);
        result
    });
    if result.is_err() {
        tracing::warn!(
            event = "persistence.event_dropped",
            sink = "task_state",
            error_kind = "sink_panicked",
            "persistence event dropped"
        );
        false
    } else {
        true
    }
}

fn assert_state_sink_is_not_mutating_state() {
    IN_STATE_SINK_CALLBACK.with(|active| {
        assert!(
            !active.get(),
            "TaskStateSink callbacks must not mutate TaskState"
        );
    });
}

pub(crate) fn assert_persistence_sink_is_not_shutting_down() {
    let in_state_callback = IN_STATE_SINK_CALLBACK.with(Cell::get);
    let in_output_callback = IN_OUTPUT_SINK_CALLBACK.with(Cell::get);
    assert!(
        !in_state_callback && !in_output_callback,
        "persistence sink callbacks must not call SupervisorApi::shutdown"
    );
}

fn deliver_output_event(sink: &TaskOutputSinkHandle, event: &TaskOutputEvent) -> bool {
    let result = IN_OUTPUT_SINK_CALLBACK.with(|active| {
        let was_active = active.replace(true);
        debug_assert!(!was_active);
        let result = catch_unwind(AssertUnwindSafe(|| sink.on_event(event)));
        active.set(false);
        result
    });
    if result.is_err() {
        tracing::warn!(
            event = "persistence.event_dropped",
            sink = "task_output",
            error_kind = "sink_panicked",
            "persistence event dropped"
        );
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant, SystemTime};

    use solti_model::{OutputEvent, TaskId};

    use super::*;

    struct PanickingStateSink;

    impl TaskStateSink for PanickingStateSink {
        fn on_event(&self, _event: &TaskStateEvent) {
            panic!("state sink panic");
        }
    }

    struct PanickingOutputSink;

    impl TaskOutputSink for PanickingOutputSink {
        fn on_event(&self, _event: &TaskOutputEvent) {
            panic!("output sink panic");
        }
    }

    struct IgnoringStateSink;

    impl TaskStateSink for IgnoringStateSink {
        fn on_event(&self, _event: &TaskStateEvent) {}
    }

    struct BlockingStateSink {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
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
                self.release.lock().recv().unwrap();
            }
        }
    }

    struct PanickingFirstOutputSink {
        first: AtomicBool,
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl TaskOutputSink for PanickingFirstOutputSink {
        fn on_event(&self, _event: &TaskOutputEvent) {
            if self.first.swap(false, Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.release.lock().recv().unwrap();
                panic!("first output callback panic");
            }
        }
    }

    fn output_event(name: &str) -> TaskOutputEvent {
        TaskOutputEvent::new(
            TaskId::new(name).unwrap(),
            Uid::new(format!("{name}-uid")).unwrap(),
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                started_at: SystemTime::UNIX_EPOCH,
            },
        )
    }

    impl TaskStateSink for BlockingStateSink {
        fn on_event(&self, _event: &TaskStateEvent) {
            self.entered.send(()).unwrap();
            self.release.lock().recv().unwrap();
        }
    }

    #[test]
    fn persistence_config_is_bounded_and_checked() {
        assert_eq!(
            PersistenceConfig::default().state_queue_capacity().get(),
            2_048
        );
        assert_eq!(
            PersistenceConfig::default().output_queue_capacity().get(),
            2_048
        );
        assert_eq!(
            PersistenceConfig::new()
                .try_with_state_queue_capacity(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "persistence_state_queue_capacity"
            }
        );
        assert_eq!(
            PersistenceConfig::new()
                .try_with_state_queue_capacity(1)
                .unwrap_err(),
            ConfigError::BelowMinimum {
                field: "persistence_state_queue_capacity",
                minimum: 2,
            }
        );
        assert_eq!(
            PersistenceConfig::new()
                .try_with_state_queue_capacity(usize::MAX)
                .unwrap_err(),
            ConfigError::Exceeds {
                field: "persistence_state_queue_capacity",
                limit: "persistence_state_queue_capacity_max",
            }
        );
        assert_eq!(
            PersistenceConfig::new()
                .try_with_state_queue_capacity(7)
                .unwrap()
                .state_queue_capacity()
                .get(),
            7
        );
        assert_eq!(
            PersistenceConfig::new()
                .try_with_output_queue_capacity(0)
                .unwrap_err(),
            ConfigError::Zero {
                field: "persistence_output_queue_capacity"
            }
        );
        assert_eq!(
            PersistenceConfig::new()
                .try_with_output_queue_capacity(7)
                .unwrap()
                .output_queue_capacity()
                .get(),
            7
        );
    }

    #[tokio::test]
    async fn event_reservations_are_atomic_and_fifo() {
        let sink: TaskStateSinkHandle = Arc::new(IgnoringStateSink);
        let dispatcher =
            Arc::new(StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap());
        let active_event = dispatcher
            .reserve(1)
            .pop()
            .expect("one event reservation returns one permit");
        assert_eq!(
            dispatcher.status(),
            TaskStateSinkStatus {
                accepting: true,
                healthy: true,
                queued: 1,
                capacity: 3,
                delivered: 0,
                failed: 0,
            }
        );

        let (large_done_tx, large_done_rx) = mpsc::sync_channel(1);
        let large_dispatcher = Arc::clone(&dispatcher);
        let large = thread::spawn(move || {
            assert!(
                large_done_tx.send(large_dispatcher.reserve(3)).is_ok(),
                "the large reservation receiver must remain open"
            );
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while dispatcher.permits.state.lock().waiters.len() != 1 {
            assert!(
                Instant::now() < deadline,
                "the large reservation must queue"
            );
            thread::yield_now();
        }

        let (small_done_tx, small_done_rx) = mpsc::sync_channel(1);
        let small_dispatcher = Arc::clone(&dispatcher);
        let small = thread::spawn(move || {
            assert!(
                small_done_tx.send(small_dispatcher.reserve(1)).is_ok(),
                "the small reservation receiver must remain open"
            );
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while dispatcher.permits.state.lock().waiters.len() != 2 {
            assert!(
                Instant::now() < deadline,
                "the small reservation must queue"
            );
            thread::yield_now();
        }

        drop(active_event);
        let large_permits = large_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the head reservation must atomically receive all three permits");
        assert_eq!(large_permits.len(), 3);
        assert_eq!(dispatcher.status().queued(), 3);
        assert!(
            small_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a later small reservation must not bypass the FIFO head"
        );

        drop(large_permits);
        let small_permits = small_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the next FIFO reservation must resume after the head releases capacity");
        assert_eq!(small_permits.len(), 1);
        drop(small_permits);
        assert_eq!(dispatcher.status().queued(), 0);
        large.join().unwrap();
        small.join().unwrap();
        dispatcher.shutdown().await;
    }

    #[tokio::test]
    async fn sink_panics_are_isolated() {
        let state: TaskStateSinkHandle = Arc::new(PanickingStateSink);
        let dispatcher = StateEventDispatcher::start(state, NonZeroUsize::new(2).unwrap()).unwrap();
        let permit = dispatcher
            .reserve(1)
            .pop()
            .expect("one event reservation returns one permit");
        dispatcher.dispatch(vec![StateDispatchEvent::new(
            TaskStateEvent::TaskChanged {
                resource_version: "test:1".to_string(),
                previous: None,
                current: None,
            },
            permit,
        )]);
        dispatcher.shutdown().await;
        assert_eq!(
            dispatcher.status(),
            TaskStateSinkStatus {
                accepting: false,
                healthy: false,
                queued: 0,
                capacity: 3,
                delivered: 0,
                failed: 1,
            }
        );

        let output: TaskOutputSinkHandle = Arc::new(PanickingOutputSink);
        let dispatcher =
            OutputEventDispatcher::start(output, NonZeroUsize::new(2).unwrap()).unwrap();
        assert!(dispatcher.try_dispatch(output_event("panic-test")));
        dispatcher.shutdown().await;
        assert_eq!(
            dispatcher.status(),
            TaskOutputSinkStatus {
                accepting: false,
                healthy: false,
                queued: 0,
                capacity: 2,
                delivered: 0,
                failed: 1,
                dropped: 0,
            }
        );
    }

    #[tokio::test]
    async fn output_capacity_counts_active_and_buffered_events() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let sink: TaskOutputSinkHandle = Arc::new(BlockingFirstOutputSink {
            first: AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let dispatcher = OutputEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();

        assert!(dispatcher.try_dispatch(output_event("active")));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first output callback must become active");
        assert!(dispatcher.try_dispatch(output_event("buffered")));
        assert!(!dispatcher.try_dispatch(output_event("dropped")));
        assert_eq!(
            dispatcher.status(),
            TaskOutputSinkStatus {
                accepting: true,
                healthy: true,
                queued: 2,
                capacity: 2,
                delivered: 0,
                failed: 0,
                dropped: 1,
            }
        );

        release_tx.send(()).unwrap();
        dispatcher.shutdown().await;
        assert_eq!(dispatcher.status().queued(), 0);
        assert_eq!(dispatcher.status().delivered(), 2);
        assert!(!dispatcher.status().accepting());
    }

    #[tokio::test]
    async fn output_callback_panic_closes_admission_and_drains_accepted_events() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let sink: TaskOutputSinkHandle = Arc::new(PanickingFirstOutputSink {
            first: AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let dispatcher = OutputEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();

        assert!(dispatcher.try_dispatch(output_event("panics")));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first output callback must become active");
        assert!(dispatcher.try_dispatch(output_event("already-accepted")));
        release_tx.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while dispatcher.status().healthy() {
            assert!(
                Instant::now() < deadline,
                "the callback panic must become visible"
            );
            thread::yield_now();
        }
        assert!(!dispatcher.try_dispatch(output_event("after-panic")));
        dispatcher.shutdown().await;
        assert_eq!(
            dispatcher.status(),
            TaskOutputSinkStatus {
                accepting: false,
                healthy: false,
                queued: 0,
                capacity: 2,
                delivered: 1,
                failed: 1,
                dropped: 1,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_worker_panic_releases_active_and_buffered_ownership() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let sink: TaskOutputSinkHandle = Arc::new(BlockingFirstOutputSink {
            first: AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let dispatcher =
            Arc::new(OutputEventDispatcher::start(sink, NonZeroUsize::new(3).unwrap()).unwrap());

        assert!(dispatcher.try_dispatch(output_event("active-before-worker-panic")));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first output callback must become active");
        assert!(dispatcher.try_dispatch(output_event("worker-panic")));
        assert!(dispatcher.try_dispatch(output_event("buffered-at-worker-panic")));
        dispatcher.inject_worker_panic();
        release_tx.send(()).unwrap();

        let shutdown_dispatcher = Arc::clone(&dispatcher);
        let shutdown = tokio::spawn(async move {
            shutdown_dispatcher.shutdown().await;
        });
        assert!(shutdown.await.unwrap_err().is_panic());
        assert_eq!(dispatcher.status().queued(), 0);
        assert_eq!(dispatcher.status().delivered(), 1);
        assert!(!dispatcher.status().accepting());
        assert!(!dispatcher.status().healthy());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_shutdown_waiter_does_not_publish_early_completion() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let sink: TaskStateSinkHandle = Arc::new(BlockingStateSink {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let dispatcher =
            Arc::new(StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap());
        let permit = dispatcher
            .reserve(1)
            .pop()
            .expect("one event reservation returns one permit");
        dispatcher.dispatch(vec![StateDispatchEvent::new(
            TaskStateEvent::TaskChanged {
                resource_version: "test:1".to_string(),
                previous: None,
                current: None,
            },
            permit,
        )]);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("state persistence callback must start");
        assert_eq!(dispatcher.status().queued(), 1);
        assert_eq!(dispatcher.status().capacity(), 3);

        let first_dispatcher = Arc::clone(&dispatcher);
        let first = tokio::spawn(async move {
            first_dispatcher.shutdown().await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dispatcher.shutdown.started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first shutdown waiter must start the drain");
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let mut second = Box::pin(dispatcher.shutdown());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), second.as_mut())
                .await
                .is_err(),
            "a later shutdown waiter must not return before the worker drains"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the later shutdown waiter must observe the completed drain");
        assert_eq!(
            dispatcher.status(),
            TaskStateSinkStatus {
                accepting: false,
                healthy: true,
                queued: 0,
                capacity: 3,
                delivered: 1,
                failed: 0,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_output_shutdown_waiter_does_not_publish_early_completion() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let sink: TaskOutputSinkHandle = Arc::new(BlockingFirstOutputSink {
            first: AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let dispatcher =
            Arc::new(OutputEventDispatcher::start(sink, NonZeroUsize::new(1).unwrap()).unwrap());
        assert!(dispatcher.try_dispatch(output_event("canceled-output-shutdown")));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the output persistence callback must start");

        let first_dispatcher = Arc::clone(&dispatcher);
        let first = tokio::spawn(async move {
            first_dispatcher.shutdown().await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dispatcher.shutdown.started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first output shutdown waiter must start the drain");
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let mut second = Box::pin(dispatcher.shutdown());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), second.as_mut())
                .await
                .is_err(),
            "a later output shutdown waiter must not return before the worker drains"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("the later output shutdown waiter must observe the completed drain");
        assert_eq!(dispatcher.status().queued(), 0);
        assert_eq!(dispatcher.status().delivered(), 1);
        assert!(!dispatcher.status().accepting());
        assert!(dispatcher.status().healthy());
    }
}
