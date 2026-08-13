//! # Persistence hooks
//!
//! These hooks let an agent forward task state and output to external storage.
//!
//! State events enter a bounded, lossless core-owned queue after the authoritative
//! state lock is released. Queue saturation applies backpressure to the commit.
//! One worker delivers callbacks in commit order and shutdown drains that queue.
//! Output remains live and best-effort.

use std::{
    cell::Cell,
    collections::VecDeque,
    io,
    num::NonZeroUsize,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use parking_lot::{Condvar, Mutex};
use solti_model::{OutputEvent, Task, TaskId, TaskRun, Uid};

use crate::ConfigError;

const DEFAULT_STATE_QUEUE_CAPACITY: NonZeroUsize = NonZeroUsize::new(2_048).unwrap();
pub(crate) const MAX_STATE_EVENTS_PER_COMMIT: usize = 3;
const MIN_STATE_QUEUE_CAPACITY: usize = MAX_STATE_EVENTS_PER_COMMIT - 1;
const MAX_STATE_QUEUE_CAPACITY: usize = usize::MAX - 1;

/// Persistence delivery settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistenceConfig {
    state_queue_capacity: NonZeroUsize,
}

impl PersistenceConfig {
    /// Creates the default bounded persistence settings.
    pub const fn new() -> Self {
        Self {
            state_queue_capacity: DEFAULT_STATE_QUEUE_CAPACITY,
        }
    }

    /// Returns `C` for the hard admission bound `reserved + buffered + active <= C + 1`.
    /// The active callback count is either zero or one.
    pub const fn state_queue_capacity(self) -> NonZeroUsize {
        self.state_queue_capacity
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
/// that does so. Reads are allowed.
pub trait TaskStateSink: Send + Sync + 'static {
    /// Receives one committed state event.
    fn on_event(&self, event: &TaskStateEvent);
}

/// Shared task state sink.
pub type TaskStateSinkHandle = Arc<dyn TaskStateSink>;

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

thread_local! {
    static IN_STATE_SINK_CALLBACK: Cell<bool> = const { Cell::new(false) };
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
    shutdown: StateDispatcherShutdown,
}

impl StateEventDispatcher {
    pub(crate) fn start(sink: TaskStateSinkHandle, capacity: NonZeroUsize) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel::<StateDispatchEvent>();
        let worker = thread::Builder::new()
            .name("solti-state-persistence".to_owned())
            .spawn(move || {
                while let Ok(dispatched) = receiver.recv() {
                    deliver_state_event(&sink, dispatched.event);
                }
            })?;
        // `capacity` counts buffered events. One additional permit belongs to
        // the callback currently executing on the persistence worker.
        let permit_limit = capacity
            .get()
            .checked_add(1)
            .expect("validated persistence capacity leaves one active event slot");
        let permits = Arc::new(StateQueuePermits {
            state: Mutex::new(StateQueuePermitState {
                available: permit_limit,
                waiters: VecDeque::new(),
            }),
            limit: permit_limit,
        });
        Ok(Self {
            sender: Mutex::new(Some(sender)),
            worker: Mutex::new(Some(worker)),
            permits,
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
            sender
                .send(event)
                .expect("state persistence worker must remain available");
        }
    }

    pub(crate) async fn shutdown(&self) {
        if self.shutdown.begin() {
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

/// Synchronous receiver of task output events.
///
/// The sink receives output from the first event because it is installed before runners start.
/// The callback is isolated from panics, but it must not block runner execution.
/// A database integration should forward the cloned event to its own worker.
pub trait TaskOutputSink: Send + Sync + 'static {
    /// Receives one task output event.
    fn on_event(&self, event: &TaskOutputEvent);
}

/// Shared task output sink.
pub type TaskOutputSinkHandle = Arc<dyn TaskOutputSink>;

pub(crate) struct PersistenceSinks {
    pub(crate) state: Option<TaskStateSinkHandle>,
    pub(crate) output: Option<TaskOutputSinkHandle>,
    pub(crate) config: PersistenceConfig,
}

fn deliver_state_event(sink: &TaskStateSinkHandle, event: TaskStateEvent) {
    let result = IN_STATE_SINK_CALLBACK.with(|active| {
        debug_assert!(!active.replace(true));
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

pub(crate) fn publish_output_event(sink: Option<&TaskOutputSinkHandle>, event: TaskOutputEvent) {
    let Some(sink) = sink else {
        return;
    };
    if catch_unwind(AssertUnwindSafe(|| sink.on_event(&event))).is_err() {
        tracing::warn!(
            event = "persistence.event_dropped",
            sink = "task_output",
            error_kind = "sink_panicked",
            "persistence event dropped"
        );
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

        let output: TaskOutputSinkHandle = Arc::new(PanickingOutputSink);
        publish_output_event(
            Some(&output),
            TaskOutputEvent::new(
                TaskId::new("panic-test").unwrap(),
                Uid::new("panic-test-uid").unwrap(),
                OutputEvent::RunStarted {
                    generation: 1,
                    attempt: 1,
                    started_at: SystemTime::UNIX_EPOCH,
                },
            ),
        );
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
    }
}
