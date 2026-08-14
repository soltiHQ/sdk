//! Bounded blocking I/O for native containerd attempts.
//!
//! One domain belongs to one containerd engine. It owns one operating-system
//! thread and a bounded queue. Filesystem preparation and removal run only on
//! that thread.
//!
//! ```text
//! cleanup admission -> I/O admission -> prepare -> ready owner
//!                                             ready owner -> remove -> release
//! ```
//!
//! The I/O limit is separate from cleanup admission. Attempt creation enters
//! this domain only after it reserves cleanup ownership. One I/O permit stays
//! charged from preparation through removal.
//!
//! Cancellation keeps the active result receiver in its operation owner. A
//! dropped owner asks the worker to remove any prepared resources. Ownership
//! is quarantined when the worker cannot report or remove it safely.

use std::{
    error::Error,
    fmt, mem,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tracing::error;

use super::io::AttemptIo;
use crate::container::{ContainerEngineError, ContainerOutput};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Blocking I/O admission and worker lifetime shared by one engine.
#[derive(Clone)]
pub(super) struct IoDomain {
    /// Shared admission, health, and worker state.
    inner: Arc<IoDomainInner>,
}

/// Shared state retained by the engine and every managed I/O owner.
struct IoDomainInner {
    /// Admission state shared with terminal shutdown.
    admission: Mutex<IoAdmission>,
    /// Sole queue sender owner shared without cloning the sender.
    queue: Arc<IoQueue>,
    /// Capacity charged for prepared and active I/O ownership.
    capacity: Arc<Semaphore>,
    /// Configured capacity used to wait for every permit.
    capacity_limit: u32,
    /// Worker handle joined only by explicit shutdown.
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    /// Worker and ownership health shared with accepted jobs.
    health: Arc<IoHealth>,
    /// Serializes terminal shutdown calls.
    shutdown: tokio::sync::Mutex<()>,
}

/// Queue and terminal state protected by one short synchronous lock.
struct IoAdmission {
    /// Whether new preparation may reserve capacity.
    accepting: bool,
}

/// Queue sender that terminal shutdown can close despite leaked owners.
struct IoQueue {
    /// Sole sender removed when terminal shutdown closes the worker queue.
    sender: Mutex<Option<SyncSender<IoJob>>>,
}

impl IoQueue {
    /// Tries to enqueue one job without waiting.
    fn try_send(&self, job: IoJob) -> Result<(), Box<TrySendError<IoJob>>> {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match sender.as_ref() {
            Some(sender) => sender.try_send(job).map_err(Box::new),
            None => Err(Box::new(TrySendError::Disconnected(job))),
        }
    }

    /// Removes the sole sender and closes the worker queue.
    fn close(&self) {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(sender);
    }
}

/// Terminal failures observed by admission and shutdown.
#[derive(Default)]
struct IoHealth {
    /// Whether the worker lost an accepted operation.
    failed: AtomicBool,
    /// Whether local I/O ownership could not be removed.
    quarantined: AtomicBool,
    /// Whether the worker unwound through its outer panic boundary.
    panicked: AtomicBool,
}

impl IoDomain {
    /// Starts one engine-local blocking I/O worker.
    ///
    /// `capacity` bounds I/O ownership separately from cleanup ownership. The
    /// caller must reserve cleanup admission before it prepares attempt I/O.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid capacity or when the worker thread cannot
    /// start.
    pub(super) fn start(capacity: usize) -> Result<Self, ContainerEngineError> {
        if capacity == 0 || capacity > Semaphore::MAX_PERMITS {
            return Err(ContainerEngineError::permanent(
                "containerd I/O capacity is outside the supported range",
            ));
        }
        let capacity_limit = u32::try_from(capacity).map_err(|error| {
            ContainerEngineError::permanent_from(
                "containerd I/O capacity exceeds shutdown counter range",
                error,
            )
        })?;
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let queue = Arc::new(IoQueue {
            sender: Mutex::new(Some(sender)),
        });
        let health = Arc::new(IoHealth::default());
        let worker_health = Arc::clone(&health);
        let worker = thread::Builder::new()
            .name("solti-containerd-io".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| run_worker(receiver)));
                if let Err(payload) = result {
                    worker_health.failed.store(true, Ordering::Release);
                    worker_health.panicked.store(true, Ordering::Release);
                    mem::forget(payload);
                }
            })
            .map_err(|error| {
                ContainerEngineError::retryable_from("cannot start containerd I/O worker", error)
            })?;

        Ok(Self {
            inner: Arc::new(IoDomainInner {
                admission: Mutex::new(IoAdmission { accepting: true }),
                queue,
                capacity: Arc::new(Semaphore::new(capacity)),
                capacity_limit,
                worker: Mutex::new(Some(worker)),
                health,
                shutdown: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Reserves capacity and queues one blocking preparation.
    ///
    /// The method never waits. The returned owner retains the preparation
    /// result receiver across cancellation of [`IoPreparation::join`].
    ///
    /// # Errors
    ///
    /// Returns a retryable error when the bounded domain is full or closed.
    /// Returns a permanent error when the worker lost safe forward progress.
    pub(super) fn try_prepare(
        &self,
        root: PathBuf,
        attempt_id: String,
    ) -> Result<IoPreparation, ContainerEngineError> {
        let admission = self
            .inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !admission.accepting {
            return Err(ContainerEngineError::retryable(
                "containerd I/O admission is closed",
            ));
        }
        ensure_worker_available(&self.inner)?;
        let permit = Arc::clone(&self.inner.capacity)
            .try_acquire_owned()
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::NoPermits => {
                    ContainerEngineError::retryable("containerd I/O admission is full")
                }
                tokio::sync::TryAcquireError::Closed => {
                    ContainerEngineError::retryable("containerd I/O admission is closed")
                }
            })?;
        let (result_sender, result_receiver) = oneshot::channel();
        let job = IoJob::prepare(
            root,
            attempt_id,
            permit,
            Arc::clone(&self.inner.queue),
            result_sender,
            Arc::clone(&self.inner.health),
        );

        match self.inner.queue.try_send(job) {
            Ok(()) => Ok(IoPreparation {
                owner: PreparationOwner::Running(result_receiver),
            }),
            Err(error) => match *error {
                TrySendError::Full(job) => {
                    self.inner.health.failed.store(true, Ordering::Release);
                    job.release_unstarted_prepare();
                    Err(ContainerEngineError::permanent(
                        "containerd I/O queue exceeded its admission invariant",
                    ))
                }
                TrySendError::Disconnected(job) => {
                    self.inner.health.failed.store(true, Ordering::Release);
                    job.release_unstarted_prepare();
                    Err(ContainerEngineError::permanent(
                        "containerd I/O worker is unavailable",
                    ))
                }
            },
        }
    }

    /// Closes admission and joins the worker within a shared deadline.
    ///
    /// The operation is terminal and idempotent. Cancellation leaves every
    /// shutdown owner in the domain for the next call. A finite `deadline` may
    /// also bound earlier engine shutdown work. `None` waits without a local
    /// deadline.
    ///
    /// # Errors
    ///
    /// Returns a retryable error when ownership or the worker does not drain
    /// before a finite `deadline`. Returns a permanent error for lost or
    /// quarantined ownership.
    pub(super) async fn shutdown_until(
        &self,
        deadline: impl Into<Option<tokio::time::Instant>>,
    ) -> Result<(), ContainerEngineError> {
        let deadline = deadline.into();
        let _shutdown = self.inner.shutdown.lock().await;
        {
            let mut admission = self
                .inner
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            admission.accepting = false;
        }

        let permits = wait_for_ownership(
            Arc::clone(&self.inner.capacity),
            self.inner.capacity_limit,
            Arc::clone(&self.inner.health),
            deadline,
        )
        .await;

        match permits {
            Ok(permits) => {
                self.inner.queue.close();
                drop(permits);
                wait_for_shared_thread(&self.inner.worker, deadline).await?;
                health_result(&self.inner.health)
            }
            Err(error) if health_failed(&self.inner.health) => {
                self.inner.queue.close();
                let _join_result = join_shared_thread_if_finished(&self.inner.worker);
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

/// Cancellation-safe owner of one queued I/O preparation.
pub(super) struct IoPreparation {
    /// Result receiver retained until completion or terminal loss.
    owner: PreparationOwner,
}

/// State of one queued I/O preparation.
enum PreparationOwner {
    /// The worker is running or has a result to collect.
    Running(oneshot::Receiver<PrepareResult>),
    /// The worker stopped before reporting a result.
    Lost,
    /// The result was already transferred to the caller.
    Finished,
}

impl IoPreparation {
    /// Creates a test owner with a manually controlled result channel.
    #[cfg(test)]
    pub(super) fn controlled_for_test() -> (Self, TestPreparationCompleter) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                owner: PreparationOwner::Running(receiver),
            },
            TestPreparationCompleter { sender },
        )
    }

    /// Waits for preparation while retaining cancellation ownership.
    ///
    /// # Errors
    ///
    /// Returns the classified filesystem failure. Returns a permanent error
    /// when the worker loses the operation result.
    pub(super) async fn join(&mut self) -> Result<ManagedAttemptIo, ContainerEngineError> {
        let result = match &mut self.owner {
            PreparationOwner::Running(receiver) => receiver.await,
            PreparationOwner::Lost => {
                return Err(ContainerEngineError::permanent(
                    "containerd I/O worker stopped before reporting preparation",
                ));
            }
            PreparationOwner::Finished => {
                return Err(ContainerEngineError::permanent(
                    "containerd I/O preparation result was already observed",
                ));
            }
        };

        match result {
            Ok(PrepareResult::Ready(owner)) => {
                self.owner = PreparationOwner::Finished;
                Ok(owner)
            }
            Ok(PrepareResult::Failed(error)) => {
                self.owner = PreparationOwner::Finished;
                Err(error)
            }
            Ok(PrepareResult::Quarantined(error)) => {
                self.owner = PreparationOwner::Lost;
                Err(error)
            }
            Err(error) => {
                self.owner = PreparationOwner::Lost;
                Err(ContainerEngineError::permanent_from(
                    "containerd I/O worker stopped before reporting preparation",
                    error,
                ))
            }
        }
    }

    /// Returns whether preparation ownership entered terminal quarantine.
    pub(super) fn is_lost(&self) -> bool {
        matches!(self.owner, PreparationOwner::Lost)
    }
}

/// Completes one manually controlled preparation in engine lifecycle tests.
#[cfg(test)]
pub(super) struct TestPreparationCompleter {
    /// Result sender paired with the controlled preparation owner.
    sender: oneshot::Sender<PrepareResult>,
}

#[cfg(test)]
impl TestPreparationCompleter {
    /// Delivers ready test I/O to the retained preparation owner.
    pub(super) fn ready(self) {
        self.sender
            .send(PrepareResult::Ready(ManagedAttemptIo::ready_for_test()))
            .unwrap_or_else(|_| panic!("attempt state must retain the preparation receiver"));
    }
}

/// Attempt I/O whose blocking removal belongs to the engine I/O domain.
pub(super) struct ManagedAttemptIo {
    /// Ready, removing, released, or lost local ownership.
    state: ManagedIoState,
    /// Worker queue used for removal and non-blocking drop handoff.
    queue: Arc<IoQueue>,
    /// Worker health updated when handoff cannot preserve ownership.
    health: Arc<IoHealth>,
    /// Private worker used only by direct attempt-state tests.
    #[cfg(test)]
    _test_worker: Option<TestIoWorker>,
}

/// Private blocking worker retained by one direct state test owner.
#[cfg(test)]
struct TestIoWorker {
    /// Queue closed before the worker is joined.
    queue: Arc<IoQueue>,
    /// Worker handle retained until the managed owner is dropped.
    worker: Option<thread::JoinHandle<()>>,
}

#[cfg(test)]
impl Drop for TestIoWorker {
    fn drop(&mut self) {
        self.queue.close();
        if let Some(worker) = self.worker.take() {
            worker.join().expect("test I/O worker must not panic");
        }
    }
}

/// Local ownership state retained across asynchronous removal.
enum ManagedIoState {
    /// Prepared resources available to the active attempt.
    Ready(ManagedAttemptIoInner),
    /// Blocking removal is running or has a result to collect.
    Removing(RemovalOwner),
    /// All local resources and I/O capacity were released.
    Released,
    /// The worker lost safe ownership progress.
    Lost,
}

impl ManagedAttemptIo {
    /// Wraps test I/O in one private blocking worker.
    #[cfg(test)]
    pub(super) fn for_test(io: AttemptIo) -> Self {
        let (sender, receiver) = mpsc::sync_channel(1);
        let queue = Arc::new(IoQueue {
            sender: Mutex::new(Some(sender)),
        });
        let worker = thread::Builder::new()
            .name("solti-containerd-io-test".to_owned())
            .spawn(move || run_worker(receiver))
            .expect("test I/O worker must start");
        let permit = Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("test I/O capacity must be available");
        let health = Arc::new(IoHealth::default());
        Self {
            state: ManagedIoState::Ready(ManagedAttemptIoInner::new(
                io,
                permit,
                Arc::clone(&health),
            )),
            queue: Arc::clone(&queue),
            health,
            _test_worker: Some(TestIoWorker {
                queue,
                worker: Some(worker),
            }),
        }
    }

    /// Creates managed test I/O for a controlled preparation result.
    #[cfg(test)]
    pub(super) fn ready_for_test() -> Self {
        Self::for_test(AttemptIo::for_test())
    }

    /// Returns the FIFO path passed to containerd task creation.
    ///
    /// # Panics
    ///
    /// Panics when called after removal begins. The attempt lifecycle reads
    /// paths only while I/O is ready.
    pub(super) fn stdout_path(&self) -> &Path {
        self.ready_io().stdout_path()
    }

    /// Returns the error FIFO path passed to containerd task creation.
    ///
    /// # Panics
    ///
    /// Panics when called after removal begins. The attempt lifecycle reads
    /// paths only while I/O is ready.
    pub(super) fn stderr_path(&self) -> &Path {
        self.ready_io().stderr_path()
    }

    /// Activates both prepared output readers after task creation.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner is not ready or reader activation
    /// fails.
    pub(super) fn activate(&mut self) -> Result<(), ContainerEngineError> {
        match &mut self.state {
            ManagedIoState::Ready(inner) => inner
                .io_mut()
                .activate()
                .map_err(|error| io_error("cannot activate containerd output pipes", error)),
            ManagedIoState::Removing(_) | ManagedIoState::Released | ManagedIoState::Lost => Err(
                ContainerEngineError::permanent("containerd output pipes are not ready"),
            ),
        }
    }

    /// Takes the activated standard output stream.
    pub(super) fn take_stdout(&mut self) -> Option<ContainerOutput> {
        match &mut self.state {
            ManagedIoState::Ready(inner) => inner.io_mut().take_stdout(),
            ManagedIoState::Removing(_) | ManagedIoState::Released | ManagedIoState::Lost => None,
        }
    }

    /// Takes the activated standard error stream.
    pub(super) fn take_stderr(&mut self) -> Option<ContainerOutput> {
        match &mut self.state {
            ManagedIoState::Ready(inner) => inner.io_mut().take_stderr(),
            ManagedIoState::Removing(_) | ManagedIoState::Released | ManagedIoState::Lost => None,
        }
    }

    /// Removes local resources on the blocking I/O worker.
    ///
    /// Cancellation leaves a running result receiver in this owner. A failed
    /// removal restores ready ownership for a later retry.
    ///
    /// # Errors
    ///
    /// Returns a classified filesystem error. Returns a permanent error when
    /// the worker loses the removal result or cannot accept an owned job.
    pub(super) async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        if matches!(self.state, ManagedIoState::Ready(_)) {
            self.start_removal()?;
        }

        let result = match &mut self.state {
            ManagedIoState::Removing(owner) => (&mut owner.receiver).await,
            ManagedIoState::Released => return Ok(()),
            ManagedIoState::Lost => {
                return Err(ContainerEngineError::permanent(
                    "containerd I/O worker lost removal ownership",
                ));
            }
            ManagedIoState::Ready(_) => unreachable!("ready I/O starts removal before waiting"),
        };

        match result {
            Ok(RemoveResult::Released) => {
                self.state = ManagedIoState::Released;
                Ok(())
            }
            Ok(RemoveResult::Retained { inner, error }) => {
                self.state = ManagedIoState::Ready(inner);
                Err(io_error("cannot clean up containerd output pipes", error))
            }
            Err(error) => {
                self.state = ManagedIoState::Lost;
                Err(ContainerEngineError::permanent_from(
                    "containerd I/O worker stopped before reporting removal",
                    error,
                ))
            }
        }
    }

    /// Returns whether the worker lost local I/O ownership.
    pub(super) fn is_lost(&self) -> bool {
        matches!(self.state, ManagedIoState::Lost)
    }

    /// Borrows ready attempt I/O.
    fn ready_io(&self) -> &AttemptIo {
        match &self.state {
            ManagedIoState::Ready(inner) => inner.io(),
            ManagedIoState::Removing(_) | ManagedIoState::Released | ManagedIoState::Lost => {
                panic!("containerd output pipes are not ready")
            }
        }
    }

    /// Transfers ready ownership to one blocking removal job.
    fn start_removal(&mut self) -> Result<(), ContainerEngineError> {
        let ManagedIoState::Ready(inner) = mem::replace(&mut self.state, ManagedIoState::Lost)
        else {
            return Ok(());
        };
        let (result_sender, result_receiver) = oneshot::channel();
        let job = IoJob::remove(inner, result_sender, Arc::clone(&self.health));
        match self.queue.try_send(job) {
            Ok(()) => {
                self.state = ManagedIoState::Removing(RemovalOwner {
                    receiver: result_receiver,
                });
                Ok(())
            }
            Err(error) => match *error {
                TrySendError::Full(job) => {
                    self.health.failed.store(true, Ordering::Release);
                    job.quarantine("full");
                    Err(ContainerEngineError::permanent(
                        "containerd I/O removal exceeded its queue invariant",
                    ))
                }
                TrySendError::Disconnected(job) => {
                    self.health.failed.store(true, Ordering::Release);
                    job.quarantine("disconnected");
                    Err(ContainerEngineError::permanent(
                        "containerd I/O worker is unavailable during removal",
                    ))
                }
            },
        }
    }
}

impl Drop for ManagedAttemptIo {
    fn drop(&mut self) {
        let state = mem::replace(&mut self.state, ManagedIoState::Released);
        let ManagedIoState::Ready(inner) = state else {
            return;
        };
        let (result_sender, result_receiver) = oneshot::channel();
        drop(result_receiver);
        let job = IoJob::remove(inner, result_sender, Arc::clone(&self.health));
        if let Err(error) = self.queue.try_send(job) {
            self.health.failed.store(true, Ordering::Release);
            match *error {
                TrySendError::Full(job) => job.quarantine("full"),
                TrySendError::Disconnected(job) => job.quarantine("disconnected"),
            }
        }
    }
}

/// Result receiver for one blocking removal.
struct RemovalOwner {
    /// Receiver retained across cancellation of the cleanup future.
    receiver: oneshot::Receiver<RemoveResult>,
}

/// Prepared resources and capacity retained as one fail-closed unit.
struct ManagedAttemptIoInner {
    /// Local output resources removed only by the I/O worker.
    io: Option<AttemptIo>,
    /// Capacity released only after confirmed local removal.
    permit: Option<OwnedSemaphorePermit>,
    /// Health marked if armed ownership is dropped without transfer.
    health: Arc<IoHealth>,
}

impl ManagedAttemptIoInner {
    /// Creates armed local ownership.
    fn new(io: AttemptIo, permit: OwnedSemaphorePermit, health: Arc<IoHealth>) -> Self {
        Self {
            io: Some(io),
            permit: Some(permit),
            health,
        }
    }

    /// Borrows local output resources.
    fn io(&self) -> &AttemptIo {
        self.io
            .as_ref()
            .expect("managed I/O retains resources until release")
    }

    /// Borrows mutable local output resources.
    fn io_mut(&mut self) -> &mut AttemptIo {
        self.io
            .as_mut()
            .expect("managed I/O retains resources until release")
    }

    /// Releases resources and capacity after confirmed removal.
    fn release(mut self) {
        let _io = self
            .io
            .take()
            .expect("managed I/O retains resources until release");
        drop(
            self.permit
                .take()
                .expect("managed I/O retains capacity until release"),
        );
    }

    /// Retains unresolved resources and capacity for the process lifetime.
    fn quarantine(mut self) {
        self.health.failed.store(true, Ordering::Release);
        self.health.quarantined.store(true, Ordering::Release);
        let io = self
            .io
            .take()
            .expect("managed I/O retains resources until quarantine");
        let permit = self
            .permit
            .take()
            .expect("managed I/O retains capacity until quarantine");
        quarantine_io(io);
        mem::forget(permit);
    }
}

impl Drop for ManagedAttemptIoInner {
    fn drop(&mut self) {
        if self.io.is_some() || self.permit.is_some() {
            self.health.failed.store(true, Ordering::Release);
            self.health.quarantined.store(true, Ordering::Release);
        }
        if let Some(io) = self.io.take() {
            quarantine_io(io);
        }
        if let Some(permit) = self.permit.take() {
            mem::forget(permit);
        }
    }
}

/// Retains Linux filesystem ownership for the process lifetime.
#[cfg(target_os = "linux")]
fn quarantine_io(io: AttemptIo) {
    mem::forget(io);
}

/// Releases the resource-free platform placeholder.
#[cfg(not(target_os = "linux"))]
fn quarantine_io(_io: AttemptIo) {}

/// Result of one blocking preparation.
enum PrepareResult {
    /// Prepared I/O and its capacity owner.
    Ready(ManagedAttemptIo),
    /// Preparation failed without unresolved local ownership.
    Failed(ContainerEngineError),
    /// Preparation failed and local ownership entered quarantine.
    Quarantined(ContainerEngineError),
}

/// Result of one blocking removal.
enum RemoveResult {
    /// Local ownership and capacity were released.
    Released,
    /// Removal failed and ownership remains available for retry.
    Retained {
        /// Local resources and capacity preserved by the worker.
        inner: ManagedAttemptIoInner,
        /// Filesystem failure returned by blocking removal.
        error: std::io::Error,
    },
}

/// One accepted blocking worker command.
struct IoJob {
    /// Armed command retained until one deliberate worker outcome.
    inner: Option<IoJobInner>,
    /// Health marked when an armed command is lost.
    health: Arc<IoHealth>,
}

/// Operation stored in one bounded queue slot.
enum IoJobInner {
    /// Filesystem preparation for a new attempt.
    Prepare(PrepareJob),
    /// Filesystem removal for an existing attempt.
    Remove(RemoveJob),
}

impl IoJob {
    /// Creates one armed preparation command.
    fn prepare(
        root: PathBuf,
        attempt_id: String,
        permit: OwnedSemaphorePermit,
        queue: Arc<IoQueue>,
        result: oneshot::Sender<PrepareResult>,
        health: Arc<IoHealth>,
    ) -> Self {
        Self {
            inner: Some(IoJobInner::Prepare(PrepareJob {
                root,
                attempt_id,
                permit: Some(permit),
                queue,
                result: Some(result),
            })),
            health,
        }
    }

    /// Creates one armed removal command.
    fn remove(
        inner: ManagedAttemptIoInner,
        result: oneshot::Sender<RemoveResult>,
        health: Arc<IoHealth>,
    ) -> Self {
        Self {
            inner: Some(IoJobInner::Remove(RemoveJob {
                inner: Some(inner),
                result: Some(result),
            })),
            health,
        }
    }

    /// Runs one command and reports whether the worker may continue.
    fn run(&mut self) -> WorkerProgress {
        match self
            .inner
            .as_mut()
            .expect("I/O job remains armed until one worker outcome")
        {
            IoJobInner::Prepare(job) => run_prepare(job, &self.health),
            IoJobInner::Remove(job) => run_remove(job, &self.health),
        }
    }

    /// Releases a preparation permit when its command was never accepted.
    fn release_unstarted_prepare(mut self) {
        let inner = self
            .inner
            .take()
            .expect("unstarted I/O preparation remains armed");
        match inner {
            IoJobInner::Prepare(job) => job.release(),
            IoJobInner::Remove(job) => {
                job.quarantine();
                panic!("removal job cannot use unstarted preparation release")
            }
        }
    }

    /// Quarantines owned state after a removal handoff invariant fails.
    fn quarantine(mut self, reason: &'static str) {
        self.health.failed.store(true, Ordering::Release);
        self.health.quarantined.store(true, Ordering::Release);
        if let Some(inner) = self.inner.take() {
            inner.quarantine();
        }
        error!(
            event = "containerd.io_handoff_failed",
            reason, "containerd I/O ownership was quarantined",
        );
    }

    /// Disarms a completed command.
    fn disarm(&mut self) {
        self.inner.take();
    }
}

impl Drop for IoJob {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        self.health.failed.store(true, Ordering::Release);
        inner.quarantine();
    }
}

impl IoJobInner {
    /// Retains ownership carried by a lost command.
    fn quarantine(self) {
        match self {
            Self::Prepare(job) => job.quarantine(),
            Self::Remove(job) => job.quarantine(),
        }
    }
}

/// Filesystem preparation and its result owner.
struct PrepareJob {
    /// Validated root requested by engine configuration.
    root: PathBuf,
    /// Opaque attempt identifier used only by I/O naming policy.
    attempt_id: String,
    /// Capacity retained until preparation becomes managed ownership.
    permit: Option<OwnedSemaphorePermit>,
    /// Shared queue owner transferred to prepared managed ownership.
    queue: Arc<IoQueue>,
    /// Result channel returned to the preparation owner.
    result: Option<oneshot::Sender<PrepareResult>>,
}

impl PrepareJob {
    /// Releases capacity for a command that created no resources.
    fn release(mut self) {
        drop(
            self.permit
                .take()
                .expect("preparation retains capacity until release"),
        );
    }

    /// Retains capacity after the worker loses this command.
    fn quarantine(mut self) {
        if let Some(permit) = self.permit.take() {
            mem::forget(permit);
        }
    }
}

impl Drop for PrepareJob {
    fn drop(&mut self) {
        if let Some(permit) = self.permit.take() {
            mem::forget(permit);
        }
    }
}

/// Local removal and its result owner.
struct RemoveJob {
    /// Local ownership retained until release or result transfer.
    inner: Option<ManagedAttemptIoInner>,
    /// Result channel returned to the managed owner.
    result: Option<oneshot::Sender<RemoveResult>>,
}

impl RemoveJob {
    /// Retains local ownership after the worker loses this command.
    fn quarantine(mut self) {
        if let Some(inner) = self.inner.take() {
            inner.quarantine();
        }
    }
}

impl Drop for RemoveJob {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.quarantine();
        }
    }
}

/// Whether the worker may receive another operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerProgress {
    /// The previous operation reached a safe result.
    Continue,
    /// Ownership was quarantined and the worker must stop.
    Stop,
}

/// Runs accepted operations serially on the dedicated thread.
fn run_worker(receiver: mpsc::Receiver<IoJob>) {
    while let Ok(mut job) = receiver.recv() {
        let progress = job.run();
        job.disarm();
        if progress == WorkerProgress::Stop {
            return;
        }
    }
}

/// Prepares one attempt and handles caller loss before result transfer.
fn run_prepare(job: &mut PrepareJob, health: &Arc<IoHealth>) -> WorkerProgress {
    match AttemptIo::prepare_retained(&job.root, &job.attempt_id) {
        Ok(io) => {
            let permit = job
                .permit
                .take()
                .expect("preparation retains capacity until ownership transfer");
            let managed = ManagedAttemptIo {
                state: ManagedIoState::Ready(ManagedAttemptIoInner::new(
                    io,
                    permit,
                    Arc::clone(health),
                )),
                queue: Arc::clone(&job.queue),
                health: Arc::clone(health),
                #[cfg(test)]
                _test_worker: None,
            };
            let result = job
                .result
                .take()
                .expect("preparation retains its result sender until completion");
            match result.send(PrepareResult::Ready(managed)) {
                Ok(()) => WorkerProgress::Continue,
                Err(PrepareResult::Ready(mut managed)) => {
                    remove_or_quarantine_managed(&mut managed)
                }
                Err(PrepareResult::Failed(_) | PrepareResult::Quarantined(_)) => {
                    unreachable!("successful preparation cannot return a failed result")
                }
            }
        }
        Err(error) => {
            let (prepare_error, partial) = error.into_parts();
            match partial {
                Some(io) => finish_failed_partial_prepare(job, health, prepare_error, io),
                None => {
                    let result = job
                        .result
                        .take()
                        .expect("preparation retains its result sender until completion");
                    drop(
                        job.permit
                            .take()
                            .expect("preparation retains capacity until failure"),
                    );
                    let _ = result.send(PrepareResult::Failed(io_error(
                        "cannot prepare containerd output pipes",
                        prepare_error,
                    )));
                    WorkerProgress::Continue
                }
            }
        }
    }
}

/// Removes partial preparation or quarantines its retained owner.
fn finish_failed_partial_prepare(
    job: &mut PrepareJob,
    health: &Arc<IoHealth>,
    prepare_error: std::io::Error,
    io: AttemptIo,
) -> WorkerProgress {
    let permit = job
        .permit
        .take()
        .expect("partial preparation retains capacity until removal");
    let mut inner = ManagedAttemptIoInner::new(io, permit, Arc::clone(health));
    match inner.io_mut().cleanup_blocking() {
        Ok(()) => {
            inner.release();
            let result = job
                .result
                .take()
                .expect("preparation retains its result sender until completion");
            let _ = result.send(PrepareResult::Failed(io_error(
                "cannot prepare containerd output pipes",
                prepare_error,
            )));
            WorkerProgress::Continue
        }
        Err(cleanup_error) => {
            health.quarantined.store(true, Ordering::Release);
            inner.quarantine();
            let result = job
                .result
                .take()
                .expect("preparation retains its result sender until completion");
            let _ = result.send(PrepareResult::Quarantined(
                ContainerEngineError::permanent_from(
                    "containerd output preparation failed and partial I/O could not be removed",
                    PrepareRollbackFailure {
                        prepare: prepare_error,
                        cleanup: cleanup_error,
                    },
                ),
            ));
            WorkerProgress::Stop
        }
    }
}

/// Removes one managed owner and preserves it when removal fails.
fn run_remove(job: &mut RemoveJob, health: &Arc<IoHealth>) -> WorkerProgress {
    let cleanup = job
        .inner
        .as_mut()
        .expect("removal retains local ownership until completion")
        .io_mut()
        .cleanup_blocking();
    let result = job
        .result
        .take()
        .expect("removal retains its result sender until completion");

    match cleanup {
        Ok(()) => {
            job.inner
                .take()
                .expect("successful removal retains ownership until release")
                .release();
            let _ = result.send(RemoveResult::Released);
            WorkerProgress::Continue
        }
        Err(error) => {
            let retained = RemoveResult::Retained {
                inner: job
                    .inner
                    .take()
                    .expect("failed removal retains ownership for retry"),
                error,
            };
            match result.send(retained) {
                Ok(()) => WorkerProgress::Continue,
                Err(RemoveResult::Retained { inner, error }) => {
                    health.quarantined.store(true, Ordering::Release);
                    inner.quarantine();
                    error!(
                        event = "containerd.io_orphan_cleanup_failed",
                        error = %error,
                        "orphaned containerd I/O ownership was quarantined",
                    );
                    WorkerProgress::Stop
                }
                Err(RemoveResult::Released) => {
                    unreachable!("failed removal cannot return a released result")
                }
            }
        }
    }
}

/// Removes a prepared result whose caller disappeared before transfer.
fn remove_or_quarantine_managed(managed: &mut ManagedAttemptIo) -> WorkerProgress {
    let state = mem::replace(&mut managed.state, ManagedIoState::Lost);
    let ManagedIoState::Ready(mut inner) = state else {
        unreachable!("fresh preparation returns ready ownership")
    };
    match inner.io_mut().cleanup_blocking() {
        Ok(()) => {
            inner.release();
            WorkerProgress::Continue
        }
        Err(error) => {
            managed.health.quarantined.store(true, Ordering::Release);
            inner.quarantine();
            error!(
                event = "containerd.io_orphan_prepare_cleanup_failed",
                error = %error,
                "orphaned prepared containerd I/O was quarantined",
            );
            WorkerProgress::Stop
        }
    }
}

/// Verifies worker health before accepting new ownership.
fn ensure_worker_available(inner: &IoDomainInner) -> Result<(), ContainerEngineError> {
    if inner.health.panicked.load(Ordering::Acquire)
        || inner.health.failed.load(Ordering::Acquire)
        || inner.health.quarantined.load(Ordering::Acquire)
    {
        return Err(health_error(&inner.health));
    }
    let finished = inner
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(thread::JoinHandle::is_finished);
    if finished {
        inner.health.failed.store(true, Ordering::Release);
        return Err(ContainerEngineError::permanent(
            "containerd I/O worker is unavailable",
        ));
    }
    Ok(())
}

/// Waits for every I/O permit or one terminal worker failure.
async fn wait_for_ownership(
    capacity: Arc<Semaphore>,
    capacity_limit: u32,
    health: Arc<IoHealth>,
    deadline: Option<tokio::time::Instant>,
) -> Result<OwnedSemaphorePermit, ContainerEngineError> {
    let permits = capacity.acquire_many_owned(capacity_limit);
    tokio::pin!(permits);

    loop {
        if health_failed(&health) {
            return Err(health_error(&health));
        }
        tokio::select! {
            biased;
            result = &mut permits => {
                return result.map_err(|error| {
                    ContainerEngineError::retryable_from(
                        "containerd I/O admission closed during shutdown",
                        error,
                    )
                });
            }
            () = sleep_until_deadline(deadline) => {
                return Err(ContainerEngineError::retryable(
                    "containerd I/O shutdown deadline exceeded",
                ));
            }
            () = tokio::time::sleep(WORKER_POLL_INTERVAL) => {}
        }
    }
}

/// Joins a finished worker without moving its handle across cancellation.
async fn wait_for_shared_thread(
    worker: &Mutex<Option<thread::JoinHandle<()>>>,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), ContainerEngineError> {
    loop {
        let finished = worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished);
        if finished {
            break;
        }
        tokio::select! {
            biased;
            () = sleep_until_deadline(deadline) => {
                return Err(ContainerEngineError::retryable(
                    "containerd I/O shutdown deadline exceeded",
                ));
            }
            () = tokio::time::sleep(WORKER_POLL_INTERVAL) => {}
        }
    }

    let worker = worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(worker) = worker {
        worker.join().map_err(|payload| {
            mem::forget(payload);
            ContainerEngineError::permanent("containerd I/O worker panicked")
        })?;
    }
    Ok(())
}

/// Sleeps until a finite deadline or remains pending for an unbounded wait.
async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Joins a shared worker only when it has already finished.
fn join_shared_thread_if_finished(
    worker: &Mutex<Option<thread::JoinHandle<()>>>,
) -> Result<(), ContainerEngineError> {
    let worker = {
        let mut worker = worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Ok(());
        }
        worker.take()
    };
    if let Some(worker) = worker {
        worker.join().map_err(|payload| {
            mem::forget(payload);
            ContainerEngineError::permanent("containerd I/O worker panicked")
        })?;
    }
    Ok(())
}

/// Returns whether shutdown must stop waiting for permits.
fn health_failed(health: &IoHealth) -> bool {
    health.failed.load(Ordering::Acquire)
        || health.quarantined.load(Ordering::Acquire)
        || health.panicked.load(Ordering::Acquire)
}

/// Converts current terminal worker health to one stable error.
fn health_error(health: &IoHealth) -> ContainerEngineError {
    if health.panicked.load(Ordering::Acquire) {
        ContainerEngineError::permanent("containerd I/O worker panicked")
    } else if health.quarantined.load(Ordering::Acquire) {
        ContainerEngineError::permanent("containerd I/O ownership is quarantined")
    } else {
        ContainerEngineError::permanent("containerd I/O worker lost ownership progress")
    }
}

/// Returns successful health after terminal worker join.
fn health_result(health: &IoHealth) -> Result<(), ContainerEngineError> {
    if health_failed(health) {
        Err(health_error(health))
    } else {
        Ok(())
    }
}

/// Converts an attempt I/O failure to its engine classification.
fn io_error(reason: &'static str, error: std::io::Error) -> ContainerEngineError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::Unsupported
    ) {
        ContainerEngineError::permanent_from(reason, error)
    } else {
        ContainerEngineError::retryable_from(reason, error)
    }
}

/// Preparation and partial-removal failures preserved as one source.
#[derive(Debug)]
struct PrepareRollbackFailure {
    /// Filesystem error returned by preparation.
    prepare: std::io::Error,
    /// Filesystem error returned while removing partial resources.
    cleanup: std::io::Error,
}

impl fmt::Display for PrepareRollbackFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "preparation failed: {}; partial removal failed: {}",
            self.prepare, self.cleanup
        )
    }
}

impl Error for PrepareRollbackFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.prepare)
    }
}

#[cfg(test)]
mod tests;
