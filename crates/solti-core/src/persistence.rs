//! # Persistence hooks
//!
//! These hooks let an agent forward task state and output to external storage.
//!
//! State writes reserve bounded, lossless queue ownership before entering their
//! authoritative state critical section. Tokio-owned paths await fair admission;
//! Taskvisor callback workers use the same admission future on their dedicated
//! threads. Events enter the queue after the state lock is released. One worker
//! delivers callbacks in commit order and shutdown drains that queue. Output
//! callbacks use a separate bounded, nonblocking, best-effort dispatcher.

use std::{
    cell::Cell,
    future::Future,
    io,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll, Wake, Waker},
    thread,
};

use parking_lot::Mutex;
use solti_model::{OutputEvent, Task, TaskId, TaskRun, Uid};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::ConfigError;

const DEFAULT_STATE_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2_048).unwrap();
const DEFAULT_OUTPUT_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2_048).unwrap();
pub(crate) const MAX_STATE_EVENTS_PER_COMMIT: usize = 3;
const MIN_STATE_QUEUE_CAPACITY: usize = MAX_STATE_EVENTS_PER_COMMIT - 1;
const MAX_STATE_QUEUE_CAPACITY: usize = Semaphore::MAX_PERMITS - 1;

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
    /// would exceed Tokio's safe semaphore capacity.
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
/// applies fair admission backpressure before Tokio-owned state commits enter
/// their state or spawn critical sections; events are not dropped for overload.
/// The callback must eventually return so shutdown can drain the queue.
/// It must not mutate `TaskState`, directly or by waiting for another thread
/// that does so. Reads and waits for unrelated Tokio work are allowed. Polling
/// [`crate::SupervisorApi::shutdown`] on the callback worker panics before
/// shutdown starts. Waiting for another thread that calls shutdown can deadlock
/// and is also forbidden.
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
    /// Reservations that crossed the admission boundary before this becomes
    /// false remain accepted and are drained by shutdown.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StateAdmissionClosed;

pub(crate) struct StateDispatcherAdmission {
    sender: mpsc::Sender<StateDispatchEvent>,
    permits: Vec<StateQueuePermit>,
    // This field is last so a dropped admission closes its sender clone before
    // the final dispatcher owner can synchronously join the worker.
    _dispatcher_lifetime: Arc<StateEventDispatcher>,
}

impl StateDispatcherAdmission {
    pub(crate) fn take_permit(&mut self) -> StateQueuePermit {
        self.permits
            .pop()
            .expect("a state mutation must reserve its maximum event count before the write lock")
    }

    pub(crate) fn release_unused_permits(&mut self) {
        self.permits.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.permits.len()
    }
}

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
    // Tokio's fair semaphore orders both async callers and the dedicated-thread
    // adapter below through the same `acquire_many_owned` wait queue.
    semaphore: Arc<Semaphore>,
    limit: usize,
    metrics: Arc<StateDispatcherMetrics>,
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
        self.permits.semaphore.add_permits(1);
    }
}

impl StateQueuePermits {
    fn validate_request(&self, requested: usize) {
        assert!(
            requested <= self.limit,
            "an atomic state commit must fit within persistence event capacity"
        );
        assert!(
            requested <= u32::MAX as usize,
            "an atomic state commit must fit Tokio semaphore acquisition"
        );
    }

    async fn reserve(self: &Arc<Self>, requested: usize) -> Vec<StateQueuePermit> {
        self.validate_request(requested);
        if requested == 0 {
            return Vec::new();
        }
        let acquired = Arc::clone(&self.semaphore)
            .acquire_many_owned(requested as u32)
            .await
            .expect("state persistence admission remains open while commits are accepted");
        self.make_permits(requested, acquired)
    }

    #[cfg(test)]
    fn reserve_blocking_after_pending(
        self: &Arc<Self>,
        requested: usize,
        pending: mpsc::SyncSender<()>,
    ) -> Vec<StateQueuePermit> {
        block_on_thread_after_pending(self.reserve(requested), pending)
    }

    fn make_permits(
        self: &Arc<Self>,
        count: usize,
        acquired: OwnedSemaphorePermit,
    ) -> Vec<StateQueuePermit> {
        self.metrics
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued
                    .checked_add(count)
                    .filter(|updated| *updated <= self.limit)
            })
            .expect("state persistence outstanding count must remain bounded");
        acquired.forget();
        (0..count)
            .map(|_| StateQueuePermit {
                permits: Arc::clone(self),
            })
            .collect()
    }
}

struct ThreadUnparker(thread::Thread);

impl Wake for ThreadUnparker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

pub(crate) fn block_on_thread<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadUnparker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

#[cfg(test)]
fn block_on_thread_after_pending<F: Future>(future: F, pending: mpsc::SyncSender<()>) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadUnparker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(_) => panic!("the test reservation must wait for semaphore capacity"),
        Poll::Pending => pending
            .send(())
            .expect("the test must observe the queued blocking reservation"),
    }
    loop {
        thread::park();
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
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
    #[cfg(test)]
    admission_waiters: AtomicUsize,
}

#[cfg(test)]
struct StateAdmissionWaiter<'a>(&'a AtomicUsize);

#[cfg(test)]
impl StateAdmissionWaiter<'_> {
    fn new(waiters: &AtomicUsize) -> StateAdmissionWaiter<'_> {
        waiters.fetch_add(1, Ordering::AcqRel);
        StateAdmissionWaiter(waiters)
    }
}

#[cfg(test)]
impl Drop for StateAdmissionWaiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl StateEventDispatcher {
    pub(crate) fn start(
        sink: TaskStateSinkHandle,
        capacity: NonZeroUsize,
    ) -> io::Result<Arc<Self>> {
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
            semaphore: Arc::new(Semaphore::new(permit_limit)),
            limit: permit_limit,
            metrics: Arc::clone(&metrics),
        });
        Ok(Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            permits,
            metrics,
            shutdown: StateDispatcherShutdown::new(),
            #[cfg(test)]
            admission_waiters: AtomicUsize::new(0),
        }))
    }

    pub(crate) async fn reserve(
        self: &Arc<Self>,
        event_count: usize,
    ) -> Result<StateDispatcherAdmission, StateAdmissionClosed> {
        assert_state_sink_is_not_mutating_state();
        let sender = self
            .sender
            .lock()
            .as_ref()
            .cloned()
            .ok_or(StateAdmissionClosed)?;
        #[cfg(test)]
        let _waiter = StateAdmissionWaiter::new(&self.admission_waiters);
        let permits = self.permits.reserve(event_count).await;
        Ok(StateDispatcherAdmission {
            sender,
            permits,
            _dispatcher_lifetime: Arc::clone(self),
        })
    }

    pub(crate) fn reserve_blocking(
        self: &Arc<Self>,
        event_count: usize,
    ) -> Result<StateDispatcherAdmission, StateAdmissionClosed> {
        assert_state_sink_is_not_mutating_state();
        block_on_thread(self.reserve(event_count))
    }

    pub(crate) fn dispatch(&self, admission: StateDispatcherAdmission, events: StateEventBatch) {
        let StateDispatcherAdmission {
            sender,
            permits,
            _dispatcher_lifetime,
        } = admission;
        debug_assert!(
            permits.is_empty(),
            "unused state event permits must be released before dispatch"
        );
        drop(permits);
        for event in events {
            if sender.send(event).is_err() {
                self.metrics.mark_worker_failed();
                panic!("state persistence worker must remain available");
            }
        }
        drop(sender);
        drop(_dispatcher_lifetime);
    }

    pub(crate) fn status(&self) -> TaskStateSinkStatus {
        self.metrics.status()
    }

    #[cfg(test)]
    pub(crate) fn admission_waiters(&self) -> usize {
        self.admission_waiters.load(Ordering::Acquire)
    }

    pub(crate) async fn shutdown(&self) {
        if self.shutdown.begin() {
            {
                let mut sender = self.sender.lock();
                self.metrics.accepting.store(false, Ordering::Release);
                sender.take();
            }
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

    struct BlockingFirstStateSink {
        first: AtomicBool,
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

    impl TaskStateSink for BlockingFirstStateSink {
        fn on_event(&self, _event: &TaskStateEvent) {
            if self.first.swap(false, Ordering::AcqRel) {
                self.entered.send(()).unwrap();
                self.release.lock().recv().unwrap();
            }
        }
    }

    fn state_event(resource_version: &str) -> TaskStateEvent {
        TaskStateEvent::TaskChanged {
            resource_version: resource_version.to_string(),
            previous: None,
            current: None,
        }
    }

    fn dispatch_state_event(
        dispatcher: &StateEventDispatcher,
        mut admission: StateDispatcherAdmission,
        resource_version: &str,
    ) {
        let permit = admission.take_permit();
        dispatcher.dispatch(
            admission,
            vec![StateDispatchEvent::new(
                state_event(resource_version),
                permit,
            )],
        );
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_and_blocking_event_reservations_are_atomic_and_fifo() {
        let sink: TaskStateSinkHandle = Arc::new(IgnoringStateSink);
        let dispatcher = StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();
        let mut active_admission = dispatcher.reserve(1).await.unwrap();
        let active_event = active_admission.take_permit();
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

        let (large_pending_tx, large_pending_rx) = mpsc::sync_channel(1);
        let large_dispatcher = Arc::clone(&dispatcher);
        let mut large = tokio::spawn(async move {
            let mut reservation = Box::pin(large_dispatcher.reserve(3));
            std::future::poll_fn(|context| match reservation.as_mut().poll(context) {
                Poll::Ready(_) => panic!("the head reservation must initially wait"),
                Poll::Pending => Poll::Ready(()),
            })
            .await;
            large_pending_tx
                .send(())
                .expect("the test must observe the queued async reservation");
            reservation.await
        });
        large_pending_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the async head must join the semaphore FIFO");
        assert!(!large.is_finished());

        let (small_pending_tx, small_pending_rx) = mpsc::sync_channel(1);
        let (small_done_tx, small_done_rx) = mpsc::sync_channel(1);
        let small_dispatcher = Arc::clone(&dispatcher);
        let small = thread::spawn(move || {
            let permits = small_dispatcher
                .permits
                .reserve_blocking_after_pending(1, small_pending_tx);
            small_done_tx
                .send(permits)
                .expect("the test must receive the blocking reservation");
        });
        small_pending_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the blocking adapter must join the semaphore FIFO");

        drop(active_event);
        drop(active_admission);
        let large_permits = tokio::time::timeout(Duration::from_secs(5), &mut large)
            .await
            .expect("the head reservation must atomically receive all three permits")
            .unwrap()
            .unwrap();
        assert_eq!(large_permits.len(), 3);
        assert_eq!(dispatcher.status().queued(), 3);
        assert!(
            small_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a later blocking reservation must not bypass the async FIFO head"
        );

        drop(large_permits);
        let small_permits = small_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the next FIFO reservation must resume after the head releases capacity");
        assert_eq!(small_permits.len(), 1);
        drop(small_permits);
        small.join().unwrap();
        assert_eq!(dispatcher.status().queued(), 0);
        dispatcher.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceling_a_queued_reservation_releases_all_provisional_capacity() {
        let sink: TaskStateSinkHandle = Arc::new(IgnoringStateSink);
        let dispatcher = StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();
        let mut active_admission = dispatcher.reserve(1).await.unwrap();
        let active_event = active_admission.take_permit();

        let (pending_tx, pending_rx) = mpsc::sync_channel(1);
        let waiting_dispatcher = Arc::clone(&dispatcher);
        let waiting = tokio::spawn(async move {
            let mut reservation = Box::pin(waiting_dispatcher.reserve(3));
            std::future::poll_fn(|context| match reservation.as_mut().poll(context) {
                Poll::Ready(_) => panic!("the oversized reservation must initially wait"),
                Poll::Pending => Poll::Ready(()),
            })
            .await;
            pending_tx
                .send(())
                .expect("the test must observe the queued reservation");
            reservation.await
        });
        pending_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the reservation must enter the semaphore FIFO");
        waiting.abort();
        match waiting.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("the queued reservation must be canceled"),
        }

        let available = tokio::time::timeout(Duration::from_secs(5), dispatcher.reserve(2))
            .await
            .expect("canceling the head must return its provisional capacity")
            .unwrap();
        assert_eq!(available.len(), 2);
        assert_eq!(dispatcher.status().queued(), 3);
        drop(available);
        drop(active_event);
        drop(active_admission);
        assert_eq!(dispatcher.status().queued(), 0);
        dispatcher.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admission_started_before_shutdown_dispatches_and_shutdown_waits_for_it() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let sink: TaskStateSinkHandle = Arc::new(BlockingFirstStateSink {
            first: AtomicBool::new(true),
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let dispatcher = StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();

        for revision in 1..=3 {
            let admission = dispatcher.reserve(1).await.unwrap();
            dispatch_state_event(&dispatcher, admission, &format!("test:{revision}"));
            if revision == 1 {
                entered_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("the first callback must hold active ownership");
            }
        }
        assert_eq!(dispatcher.status().queued(), 3);

        let (pending_tx, pending_rx) = mpsc::sync_channel(1);
        let pending_dispatcher = Arc::clone(&dispatcher);
        let pending = tokio::spawn(async move {
            let mut admission = Box::pin(pending_dispatcher.reserve(1));
            std::future::poll_fn(|context| match admission.as_mut().poll(context) {
                Poll::Ready(_) => panic!("the accepted admission must initially wait"),
                Poll::Pending => Poll::Ready(()),
            })
            .await;
            pending_tx
                .send(())
                .expect("the test must observe the pending accepted admission");
            admission.await
        });
        pending_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the admission must clone the sender before waiting for capacity");

        let shutdown_dispatcher = Arc::clone(&dispatcher);
        let mut shutdown = tokio::spawn(async move {
            shutdown_dispatcher.shutdown().await;
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while dispatcher.sender.lock().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown must close new admission before the sink is released");
        assert!(!shutdown.is_finished());

        release_tx
            .send(())
            .expect("the test must release the active callback");
        let admission = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("the accepted admission must receive released capacity")
            .unwrap()
            .unwrap();
        assert!(!shutdown.is_finished());

        dispatch_state_event(&dispatcher, admission, "test:4");
        tokio::time::timeout(Duration::from_secs(5), &mut shutdown)
            .await
            .expect("shutdown must drain the event carried by the accepted admission")
            .unwrap();
        assert_eq!(dispatcher.status().delivered(), 4);
        assert_eq!(dispatcher.status().queued(), 0);

        let drop_dispatcher =
            StateEventDispatcher::start(Arc::new(IgnoringStateSink), NonZeroUsize::new(2).unwrap())
                .unwrap();
        let admission = drop_dispatcher.reserve(1).await.unwrap();
        drop(drop_dispatcher);
        drop(admission);
    }

    #[tokio::test]
    async fn admission_after_shutdown_is_explicitly_closed() {
        let sink: TaskStateSinkHandle = Arc::new(IgnoringStateSink);
        let dispatcher = StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();

        dispatcher.shutdown().await;

        assert!(matches!(
            dispatcher.reserve(1).await,
            Err(StateAdmissionClosed)
        ));
        assert!(matches!(
            dispatcher.reserve_blocking(0),
            Err(StateAdmissionClosed)
        ));
        assert!(!dispatcher.status().accepting());
    }

    #[tokio::test]
    async fn sink_panics_are_isolated() {
        let state: TaskStateSinkHandle = Arc::new(PanickingStateSink);
        let dispatcher = StateEventDispatcher::start(state, NonZeroUsize::new(2).unwrap()).unwrap();
        let mut admission = dispatcher.reserve(1).await.unwrap();
        let permit = admission.take_permit();
        dispatcher.dispatch(
            admission,
            vec![StateDispatchEvent::new(
                TaskStateEvent::TaskChanged {
                    resource_version: "test:1".to_string(),
                    previous: None,
                    current: None,
                },
                permit,
            )],
        );
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
        let dispatcher = StateEventDispatcher::start(sink, NonZeroUsize::new(2).unwrap()).unwrap();
        let mut admission = dispatcher.reserve(1).await.unwrap();
        let permit = admission.take_permit();
        dispatcher.dispatch(
            admission,
            vec![StateDispatchEvent::new(
                TaskStateEvent::TaskChanged {
                    resource_version: "test:1".to_string(),
                    previous: None,
                    current: None,
                },
                permit,
            )],
        );
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
