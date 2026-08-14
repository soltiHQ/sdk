//! Bounded finalization for cancelled containerd attempts.
//!
//! One domain belongs to one [`ContainerdEngine`](super::ContainerdEngine).
//! It owns an operating-system thread, a current-thread Tokio runtime, and the
//! containerd connection used by the engine.
//!
//! ```text
//! lifecycle admission -> CleanupReservation -> image resolve -> active attempt
//!                                                        |
//!                                                        v
//! cancelled attempt Drop -> bounded handoff -> cleanup runtime -> containerd
//! ```
//!
//! Admission is reserved before client-side image resolution and attempt-scoped
//! resources. Image transfer is not deferred-cleanup ownership.
//! Handoff from `Drop` never waits and does not use the caller's runtime.
//! Retryable cleanup remains charged until it succeeds. Permanent unresolved
//! ownership is quarantined and remains charged for the process lifetime.
//!
//! # Best effort
//!
//! The domain covers Rust future cancellation and runtime shutdown inside the
//! current process. It cannot clean resources after immediate process exit,
//! process abort, power loss, or `SIGKILL`.

use std::{
    mem,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use containerd_client::Client;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tracing::{error, warn};

use super::engine::AttemptState;
use crate::container::{ContainerEngineError, ContainerErrorClass};

const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(100);
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// Lifecycle admission and cleanup-worker lifetime shared by one engine.
#[derive(Clone)]
pub(super) struct CleanupDomain {
    /// Shared queue and capacity owner.
    inner: Arc<CleanupDomainInner>,
}

/// Shared state retained by the engine and every active reservation.
struct CleanupDomainInner {
    /// Admission state shared with terminal shutdown.
    admission: Mutex<CleanupAdmission>,
    /// Capacity charged before image resolution or attempt resource creation.
    capacity: Arc<Semaphore>,
    /// Configured capacity needed to wait for every owner.
    capacity_limit: u32,
    /// Worker handle joined only by explicit shutdown.
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    /// Whether the worker ended with a panic.
    worker_panicked: AtomicBool,
    /// Whether an internal cleanup task lost safe forward progress.
    cleanup_failed: Arc<AtomicBool>,
    /// Whether ownership entered permanent quarantine.
    cleanup_quarantined: Arc<AtomicBool>,
    /// Serializes terminal shutdown calls.
    shutdown: tokio::sync::Mutex<()>,
}

/// Queue and terminal state protected by one short synchronous lock.
struct CleanupAdmission {
    /// Whether new attempt ownership may be reserved.
    accepting: bool,
    /// Non-blocking handoff queue for cancelled attempts.
    sender: Option<mpsc::Sender<CleanupJob>>,
}

impl CleanupDomain {
    /// Starts an isolated cleanup runtime and connects it to containerd.
    ///
    /// The containerd channel is created inside the cleanup runtime. The same
    /// channel is returned for normal engine operations while its driver stays
    /// owned by this domain.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid capacity or when the worker thread,
    /// runtime, or containerd connection cannot start.
    pub(super) async fn start(
        socket: PathBuf,
        connect_timeout: Duration,
        capacity: usize,
    ) -> Result<(Arc<Client>, tokio::runtime::Handle, Self), ContainerEngineError> {
        if capacity == 0 || capacity > Semaphore::MAX_PERMITS {
            return Err(ContainerEngineError::permanent(
                "containerd cleanup capacity is outside the supported range",
            ));
        }
        let capacity_limit = u32::try_from(capacity).map_err(|error| {
            ContainerEngineError::permanent_from(
                "containerd cleanup capacity exceeds shutdown counter range",
                error,
            )
        })?;
        let (sender, receiver) = mpsc::channel(capacity);
        let admission = Arc::new(Semaphore::new(capacity));
        let (startup_tx, startup_rx) = oneshot::channel();
        let cleanup_failed = Arc::new(AtomicBool::new(false));
        let cleanup_quarantined = Arc::new(AtomicBool::new(false));
        let worker_failure = Arc::clone(&cleanup_failed);
        let worker_quarantine = Arc::clone(&cleanup_quarantined);

        let worker = thread::Builder::new()
            .name("solti-containerd-cleanup".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    cleanup_thread(
                        socket,
                        connect_timeout,
                        receiver,
                        startup_tx,
                        Arc::clone(&worker_failure),
                        Arc::clone(&worker_quarantine),
                    )
                }));
                if let Err(payload) = result {
                    worker_failure.store(true, Ordering::Release);
                    mem::forget(payload);
                }
            })
            .map_err(|error| {
                ContainerEngineError::retryable_from(
                    "cannot start containerd cleanup worker",
                    error,
                )
            })?;

        let startup = startup_rx.await.map_err(|error| {
            ContainerEngineError::retryable_from(
                "containerd cleanup worker stopped during startup",
                error,
            )
        });
        let (client, executor) = match startup {
            Ok(Ok(startup)) => startup,
            Ok(Err(error)) => {
                drop(sender);
                wait_for_thread(worker).await?;
                return Err(error);
            }
            Err(error) => {
                drop(sender);
                wait_for_thread(worker).await?;
                return Err(error);
            }
        };
        let domain = Self {
            inner: Arc::new(CleanupDomainInner {
                admission: Mutex::new(CleanupAdmission {
                    accepting: true,
                    sender: Some(sender),
                }),
                capacity: admission,
                capacity_limit,
                worker: Mutex::new(Some(worker)),
                worker_panicked: AtomicBool::new(false),
                cleanup_failed,
                cleanup_quarantined,
                shutdown: tokio::sync::Mutex::new(()),
            }),
        };
        Ok((client, executor, domain))
    }

    /// Reserves one lifecycle slot before image resolution starts.
    ///
    /// # Errors
    ///
    /// Returns a retryable error when every cleanup slot is already owned.
    /// Returns a permanent error when the cleanup worker cannot make progress.
    pub(super) fn try_reserve(&self) -> Result<CleanupReservation, ContainerEngineError> {
        let admission = self
            .inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !admission.accepting {
            return Err(ContainerEngineError::retryable(
                "containerd cleanup admission is closed",
            ));
        }
        let sender = admission
            .sender
            .as_ref()
            .ok_or_else(|| {
                ContainerEngineError::retryable("containerd cleanup admission is closed")
            })?
            .clone();
        let worker_finished = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished);
        if sender.is_closed()
            || worker_finished
            || self.inner.cleanup_failed.load(Ordering::Acquire)
        {
            return Err(ContainerEngineError::permanent(
                "containerd cleanup worker is unavailable",
            ));
        }
        let permit = Arc::clone(&self.inner.capacity)
            .try_acquire_owned()
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::NoPermits => {
                    ContainerEngineError::retryable("containerd cleanup admission is full")
                }
                tokio::sync::TryAcquireError::Closed => {
                    ContainerEngineError::retryable("containerd cleanup admission is closed")
                }
            })?;
        Ok(CleanupReservation {
            sender,
            permit: Some(permit),
        })
    }

    /// Closes admission, waits for every accepted owner, and joins the worker.
    ///
    /// The operation is terminal and idempotent. Existing reservations may
    /// still hand off after admission closes. The worker stops only after all
    /// accepted ownership is released. `None` waits without a local deadline.
    ///
    /// # Errors
    ///
    /// Returns a retryable error when accepted ownership does not drain before
    /// a finite `deadline`. Returns a permanent error for failed or quarantined
    /// cleanup.
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

        let all_permits = wait_for_ownership(
            Arc::clone(&self.inner.capacity),
            self.inner.capacity_limit,
            Arc::clone(&self.inner.cleanup_failed),
            Arc::clone(&self.inner.cleanup_quarantined),
            deadline,
        )
        .await?;

        let sender = self
            .inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sender
            .take();
        drop(sender);
        drop(all_permits);

        let join = wait_for_shared_thread(&self.inner.worker, &self.inner.worker_panicked);
        if let Some(deadline) = deadline {
            tokio::time::timeout_at(deadline, join)
                .await
                .map_err(|error| {
                    ContainerEngineError::retryable_from(
                        "containerd cleanup shutdown deadline exceeded",
                        error,
                    )
                })??;
        } else {
            join.await?;
        }
        Ok(())
    }

    /// Shuts down within a test-local duration.
    #[cfg(test)]
    async fn shutdown(&self, timeout: Duration) -> Result<(), ContainerEngineError> {
        self.shutdown_until(tokio::time::Instant::now().checked_add(timeout))
            .await
    }
}

/// Pre-reserved capacity for one create, active, or deferred lifecycle.
pub(super) struct CleanupReservation {
    /// Queue sender retained until explicit release or deferred handoff.
    sender: mpsc::Sender<CleanupJob>,
    /// Capacity unit transferred with the cleanup state.
    permit: Option<OwnedSemaphorePermit>,
}

impl CleanupReservation {
    /// Transfers unresolved ownership without waiting or using Tokio context.
    ///
    /// A queue failure after successful admission violates the domain
    /// invariant. The state is retained rather than destroyed on the caller's
    /// thread.
    pub(super) fn handoff(mut self, state: AttemptState) {
        let job = CleanupJob {
            inner: Some(CleanupJobInner {
                state,
                _permit: self
                    .permit
                    .take()
                    .expect("cleanup reservation owns its permit until handoff"),
            }),
        };
        match self.sender.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(job)) => {
                error!(
                    event = "containerd.cleanup_handoff_invariant_failed",
                    reason = "full",
                    ownership = %job.state().unresolved_summary(),
                    "containerd cleanup ownership could not enter its reserved queue slot",
                );
                mem::forget(job);
            }
            Err(mpsc::error::TrySendError::Closed(job)) => {
                error!(
                    event = "containerd.cleanup_handoff_invariant_failed",
                    reason = "closed",
                    ownership = %job.state().unresolved_summary(),
                    "containerd cleanup ownership could not enter a closed queue",
                );
                mem::forget(job);
            }
        }
    }
}

/// Attempt state and capacity retained until cleanup releases ownership.
struct CleanupJob {
    /// Inner ownership retained unless clean completion disarms it.
    inner: Option<CleanupJobInner>,
}

/// Attempt state and permit retained as one fail-closed unit.
struct CleanupJobInner {
    /// Mutable attempt ownership state.
    state: AttemptState,
    /// Admission unit released only after clean completion.
    _permit: OwnedSemaphorePermit,
}

impl CleanupJob {
    /// Borrows the attempt state.
    fn state(&self) -> &AttemptState {
        &self
            .inner
            .as_ref()
            .expect("cleanup job is armed until clean release")
            .state
    }

    /// Borrows the mutable attempt state.
    fn state_mut(&mut self) -> &mut AttemptState {
        &mut self
            .inner
            .as_mut()
            .expect("cleanup job is armed until clean release")
            .state
    }

    /// Releases state and capacity after complete cleanup.
    fn release(mut self) {
        drop(
            self.inner
                .take()
                .expect("cleanup job is armed until clean release"),
        );
    }
}

impl Drop for CleanupJob {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            mem::forget(inner);
        }
    }
}

/// Quarantined ownership that must not be dropped by the worker.
#[derive(Default)]
struct Quarantine {
    /// Permanently unresolved jobs retained for the process lifetime.
    jobs: Vec<CleanupJob>,
}

impl Drop for Quarantine {
    fn drop(&mut self) {
        for job in self.jobs.drain(..) {
            mem::forget(job);
        }
    }
}

/// Guard that retains a job if an internal cleanup task panics.
struct CleanupJobGuard(Option<CleanupJob>);

impl CleanupJobGuard {
    /// Borrows the guarded job.
    fn job_mut(&mut self) -> &mut CleanupJob {
        self.0
            .as_mut()
            .expect("cleanup job remains present until task completion")
    }

    /// Transfers the job out after a deliberate result.
    fn take(mut self) -> CleanupJob {
        self.0
            .take()
            .expect("cleanup job remains present until task completion")
    }
}

impl Drop for CleanupJobGuard {
    fn drop(&mut self) {
        if let Some(job) = self.0.take() {
            mem::forget(job);
        }
    }
}

/// Builds and runs the domain runtime on its owning thread.
fn cleanup_thread(
    socket: PathBuf,
    connect_timeout: Duration,
    receiver: mpsc::Receiver<CleanupJob>,
    startup: oneshot::Sender<Result<(Arc<Client>, tokio::runtime::Handle), ContainerEngineError>>,
    cleanup_failed: Arc<AtomicBool>,
    cleanup_quarantined: Arc<AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = startup.send(Err(ContainerEngineError::retryable_from(
                "cannot build containerd cleanup runtime",
                error,
            )));
            return;
        }
    };

    runtime.block_on(async move {
        let client =
            match tokio::time::timeout(connect_timeout, containerd_client::connect(socket)).await {
                Ok(Ok(channel)) => Arc::new(Client::from(channel)),
                Ok(Err(error)) => {
                    let _ = startup.send(Err(ContainerEngineError::retryable_from(
                        "cannot connect containerd cleanup runtime",
                        error,
                    )));
                    return;
                }
                Err(error) => {
                    let _ = startup.send(Err(ContainerEngineError::retryable_from(
                        "containerd cleanup connection deadline exceeded",
                        error,
                    )));
                    return;
                }
            };
        if startup
            .send(Ok((Arc::clone(&client), tokio::runtime::Handle::current())))
            .is_err()
        {
            return;
        }
        run_cleanup(receiver, cleanup_failed, cleanup_quarantined).await;
        drop(client);
    });
}

/// Waits without blocking a Tokio worker and joins a finished cleanup thread.
async fn wait_for_thread(worker: thread::JoinHandle<()>) -> Result<(), ContainerEngineError> {
    while !worker.is_finished() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    join_finished_thread(worker)
}

/// Waits for every ownership permit or a terminal cleanup failure.
async fn wait_for_ownership(
    capacity: Arc<Semaphore>,
    capacity_limit: u32,
    cleanup_failed: Arc<AtomicBool>,
    cleanup_quarantined: Arc<AtomicBool>,
    deadline: Option<tokio::time::Instant>,
) -> Result<OwnedSemaphorePermit, ContainerEngineError> {
    let permits = capacity.acquire_many_owned(capacity_limit);
    tokio::pin!(permits);

    loop {
        if cleanup_failed.load(Ordering::Acquire) {
            return Err(ContainerEngineError::permanent(
                "containerd cleanup worker lost ownership progress",
            ));
        }
        if cleanup_quarantined.load(Ordering::Acquire) {
            return Err(ContainerEngineError::permanent(
                "containerd cleanup ownership is quarantined",
            ));
        }
        tokio::select! {
            biased;
            result = &mut permits => {
                return result.map_err(|error| {
                    ContainerEngineError::retryable_from(
                        "containerd cleanup admission closed during shutdown",
                        error,
                    )
                });
            }
            () = sleep_until_deadline(deadline) => {
                return Err(ContainerEngineError::retryable(
                    "containerd cleanup shutdown deadline exceeded",
                ));
            }
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    }
}

/// Sleeps until a finite deadline or remains pending for an unbounded wait.
async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Joins a shared thread without losing its handle if this future is cancelled.
async fn wait_for_shared_thread(
    worker: &Mutex<Option<thread::JoinHandle<()>>>,
    panicked: &AtomicBool,
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
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let worker = worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match worker {
        Some(worker) => match join_finished_thread(worker) {
            Ok(()) => Ok(()),
            Err(error) => {
                panicked.store(true, Ordering::Release);
                Err(error)
            }
        },
        None if panicked.load(Ordering::Acquire) => Err(ContainerEngineError::permanent(
            "containerd cleanup worker panicked",
        )),
        None => Ok(()),
    }
}

/// Joins a thread that already reported completion.
fn join_finished_thread(worker: thread::JoinHandle<()>) -> Result<(), ContainerEngineError> {
    worker.join().map_err(|payload| {
        mem::forget(payload);
        ContainerEngineError::permanent("containerd cleanup worker panicked")
    })
}

/// Runs accepted jobs concurrently on the isolated runtime.
async fn run_cleanup(
    mut receiver: mpsc::Receiver<CleanupJob>,
    cleanup_failed: Arc<AtomicBool>,
    cleanup_quarantined: Arc<AtomicBool>,
) {
    let mut active = tokio::task::JoinSet::new();
    let mut quarantine = Quarantine::default();
    let mut input_closed = false;

    loop {
        if input_closed && active.is_empty() {
            if quarantine.jobs.is_empty() {
                return;
            }
            std::future::pending::<()>().await;
        }

        tokio::select! {
            job = receiver.recv(), if !input_closed => {
                match job {
                    Some(job) => {
                        active.spawn(run_cleanup_job(job));
                    }
                    None => input_closed = true,
                }
            }
            result = active.join_next(), if !active.is_empty() => {
                match result {
                    Some(Ok(Some(job))) => {
                        cleanup_quarantined.store(true, Ordering::Release);
                        quarantine.jobs.push(job);
                    }
                    Some(Ok(None)) => {}
                    Some(Err(error)) => {
                        cleanup_failed.store(true, Ordering::Release);
                        receiver.close();
                        input_closed = true;
                        error!(
                            event = "containerd.cleanup_task_failed",
                            error = %error,
                            "containerd cleanup task stopped unexpectedly",
                        );
                    }
                    None => {}
                }
            }
        }
    }
}

/// Repeats bounded cleanup windows until release or permanent quarantine.
async fn run_cleanup_job(job: CleanupJob) -> Option<CleanupJob> {
    let mut guard = CleanupJobGuard(Some(job));
    if let Err(error) = guard.job_mut().state_mut().settle_in_flight().await {
        error!(
            event = "containerd.cleanup_quarantined",
            error = %error,
            ownership = %guard.job_mut().state_mut().unresolved_summary(),
            "containerd mutation result was lost and ownership was quarantined",
        );
        return Some(guard.take());
    }
    let mut backoff = RETRY_BACKOFF_INITIAL;

    loop {
        match guard.job_mut().state_mut().cleanup_owned_with_retry().await {
            Ok(()) if guard.job_mut().state().is_released() => {
                guard.take().release();
                return None;
            }
            Ok(()) => {
                error!(
                    event = "containerd.cleanup_unresolved",
                    ownership = %guard.job_mut().state().unresolved_summary(),
                    "containerd cleanup returned without releasing ownership",
                );
                return Some(guard.take());
            }
            Err(error) if error.class() == ContainerErrorClass::Permanent => {
                error!(
                    event = "containerd.cleanup_quarantined",
                    error = %error,
                    ownership = %guard.job_mut().state().unresolved_summary(),
                    "containerd cleanup ownership was quarantined",
                );
                return Some(guard.take());
            }
            Err(error) => {
                warn!(
                    event = "containerd.cleanup_retry",
                    error = %error,
                    ownership = %guard.job_mut().state().unresolved_summary(),
                    "containerd deferred cleanup will retry",
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(RETRY_BACKOFF_MAX);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use tokio::sync::mpsc;

    use super::{CleanupAdmission, CleanupDomain, CleanupDomainInner};
    use crate::container::containerd::engine::cancellation_tests::test_state_for_cleanup_handoff;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Semaphore;

    pub(in crate::container::containerd) struct ObservedDomain {
        domain: CleanupDomain,
        receiver: mpsc::Receiver<super::CleanupJob>,
    }

    impl ObservedDomain {
        pub(in crate::container::containerd) fn domain(&self) -> CleanupDomain {
            self.domain.clone()
        }

        pub(in crate::container::containerd) fn assert_no_handoff(&mut self) {
            assert!(matches!(
                self.receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    pub(super) fn isolated_domain(capacity: usize) -> CleanupDomain {
        let (sender, receiver) = mpsc::channel(capacity);
        std::mem::forget(receiver);
        test_domain(sender, capacity)
    }

    fn observed_domain(capacity: usize) -> (CleanupDomain, mpsc::Receiver<super::CleanupJob>) {
        let (sender, receiver) = mpsc::channel(capacity);
        let domain = test_domain(sender, capacity);
        (domain, receiver)
    }

    fn test_domain(sender: mpsc::Sender<super::CleanupJob>, capacity: usize) -> CleanupDomain {
        CleanupDomain {
            inner: Arc::new(CleanupDomainInner {
                admission: std::sync::Mutex::new(CleanupAdmission {
                    accepting: true,
                    sender: Some(sender),
                }),
                capacity: Arc::new(Semaphore::new(capacity)),
                capacity_limit: u32::try_from(capacity).expect("test capacity must fit u32"),
                worker: std::sync::Mutex::new(None),
                worker_panicked: std::sync::atomic::AtomicBool::new(false),
                cleanup_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cleanup_quarantined: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                shutdown: tokio::sync::Mutex::new(()),
            }),
        }
    }

    pub(in crate::container::containerd) fn observed_test_domain(
        capacity: usize,
    ) -> ObservedDomain {
        let (domain, receiver) = observed_domain(capacity);
        ObservedDomain { domain, receiver }
    }

    #[test]
    fn admission_never_exceeds_the_configured_capacity() {
        let domain = isolated_domain(2);
        let first = domain.try_reserve().expect("first slot must be available");
        let second = domain.try_reserve().expect("second slot must be available");

        let error = match domain.try_reserve() {
            Ok(_) => panic!("third reservation must exceed capacity"),
            Err(error) => error,
        };
        assert_eq!(error.reason(), "containerd cleanup admission is full");

        drop(first);
        domain
            .try_reserve()
            .expect("released admission must become available");
        drop(second);
    }

    #[test]
    fn domains_have_independent_capacity() {
        let first = isolated_domain(1);
        let second = isolated_domain(1);
        let _first_slot = first.try_reserve().expect("first domain must admit");

        assert!(first.try_reserve().is_err());
        second
            .try_reserve()
            .expect("second domain must retain its own capacity");
    }

    #[test]
    fn dropping_an_uncommitted_reservation_releases_capacity() {
        let domain = isolated_domain(1);
        let reservation = domain.try_reserve().expect("slot must be available");

        drop(reservation);

        domain
            .try_reserve()
            .expect("unused reservation must release its slot");
    }

    #[test]
    fn handoff_outside_tokio_is_non_blocking_and_keeps_ownership() {
        let (domain, mut receiver) = observed_domain(1);
        let reservation = domain.try_reserve().expect("slot must be available");

        reservation.handoff(test_state_for_cleanup_handoff());

        let job = receiver
            .try_recv()
            .expect("handoff must enter the reserved queue slot");
        assert!(!job.state().is_released());
        std::mem::forget(job);
    }

    #[test]
    fn dropping_a_queued_job_keeps_its_capacity_charged() {
        let (domain, mut receiver) = observed_domain(1);
        domain
            .try_reserve()
            .expect("slot must be available")
            .handoff(test_state_for_cleanup_handoff());

        drop(receiver.try_recv().expect("handoff must enter the queue"));

        let error = match domain.try_reserve() {
            Ok(_) => panic!("fail-closed job drop must retain admission"),
            Err(error) => error,
        };
        assert_eq!(error.reason(), "containerd cleanup admission is full");
    }

    #[test]
    fn failed_cleanup_worker_rejects_new_admission() {
        let domain = isolated_domain(1);
        domain
            .inner
            .cleanup_failed
            .store(true, std::sync::atomic::Ordering::Release);

        let error = match domain.try_reserve() {
            Ok(_) => panic!("failed worker must reject admission"),
            Err(error) => error,
        };
        assert_eq!(error.reason(), "containerd cleanup worker is unavailable");
    }

    #[tokio::test]
    async fn shutdown_reports_failed_cleanup_progress_without_waiting_for_leaked_capacity() {
        let (domain, mut receiver) = observed_domain(1);
        domain
            .try_reserve()
            .expect("slot must be available")
            .handoff(test_state_for_cleanup_handoff());
        drop(receiver.try_recv().expect("handoff must enter the queue"));
        domain
            .inner
            .cleanup_failed
            .store(true, std::sync::atomic::Ordering::Release);

        let error = domain
            .shutdown(Duration::from_secs(1))
            .await
            .expect_err("failed progress must be reported as permanent");
        assert_eq!(
            error.class(),
            crate::container::ContainerErrorClass::Permanent
        );
        assert_eq!(
            error.reason(),
            "containerd cleanup worker lost ownership progress"
        );
    }

    #[tokio::test]
    async fn shutdown_reports_quarantined_ownership_without_waiting_for_deadline() {
        let domain = isolated_domain(1);
        let _reservation = domain.try_reserve().expect("slot must be available");
        domain
            .inner
            .cleanup_quarantined
            .store(true, std::sync::atomic::Ordering::Release);

        let error = domain
            .shutdown(Duration::from_secs(1))
            .await
            .expect_err("quarantined ownership must be permanent");
        assert_eq!(
            error.class(),
            crate::container::ContainerErrorClass::Permanent
        );
        assert_eq!(
            error.reason(),
            "containerd cleanup ownership is quarantined"
        );
    }

    #[tokio::test]
    async fn shutdown_is_terminal_and_idempotent() {
        let domain = isolated_domain(1);

        domain
            .shutdown(Duration::from_secs(1))
            .await
            .expect("idle domain must shut down");
        domain
            .shutdown(Duration::from_secs(1))
            .await
            .expect("completed shutdown must be idempotent");

        let error = match domain.try_reserve() {
            Ok(_) => panic!("terminal shutdown must close admission"),
            Err(error) => error,
        };
        assert_eq!(error.reason(), "containerd cleanup admission is closed");
    }

    #[tokio::test]
    async fn incomplete_shutdown_keeps_existing_ownership_and_allows_retry() {
        let domain = isolated_domain(1);
        let reservation = domain.try_reserve().expect("slot must be available");

        let error = domain
            .shutdown(Duration::from_millis(1))
            .await
            .expect_err("owned capacity must keep shutdown incomplete");
        assert_eq!(
            error.reason(),
            "containerd cleanup shutdown deadline exceeded"
        );
        assert!(domain.try_reserve().is_err());

        drop(reservation);
        domain
            .shutdown(Duration::from_secs(1))
            .await
            .expect("shutdown retry must finish after ownership release");
    }

    #[tokio::test]
    async fn huge_timeout_shutdown_can_be_canceled_and_retried() {
        let domain = isolated_domain(1);
        let reservation = domain.try_reserve().expect("slot must be available");
        let shutdown_domain = domain.clone();
        let shutdown = tokio::spawn(async move { shutdown_domain.shutdown(Duration::MAX).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let accepting = domain
                    .inner
                    .admission
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .accepting;
                if !accepting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown did not close admission");

        shutdown.abort();
        assert!(shutdown.await.unwrap_err().is_cancelled());
        drop(reservation);
        domain
            .shutdown(Duration::from_secs(1))
            .await
            .expect("shutdown retry must finish after ownership release");
    }
}
