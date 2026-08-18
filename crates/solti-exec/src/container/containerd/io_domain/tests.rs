use std::{
    future::{Future, poll_fn},
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    task::Poll,
    thread,
    time::Duration,
};

use tokio::sync::{Semaphore, oneshot};

use super::{
    IoAdmission, IoDomain, IoDomainInner, IoHealth, IoJob, IoJobInner, IoPreparation, IoQueue,
    ManagedAttemptIo, ManagedAttemptIoInner, ManagedIoState, PrepareJob, PrepareResult,
    RemovalOwner, RemoveResult, WorkerProgress, finish_failed_partial_prepare,
    remove_or_quarantine_managed,
};
use crate::container::containerd::io::AttemptIo;
use crate::container::{ContainerEngineError, ContainerErrorClass};

/// Domain whose accepted queue is controlled directly by one test.
struct ObservedDomain {
    /// Domain under test.
    domain: IoDomain,
    /// Accepted jobs not consumed by an operating-system worker.
    receiver: mpsc::Receiver<IoJob>,
}

impl ObservedDomain {
    /// Creates a healthy domain with a manually observed queue.
    fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::channel();
        let queue = Arc::new(IoQueue {
            sender: Mutex::new(Some(sender)),
        });
        let capacity_limit = u32::try_from(capacity).expect("test capacity fits u32");
        let domain = IoDomain {
            inner: Arc::new(IoDomainInner {
                admission: Mutex::new(IoAdmission { accepting: true }),
                queue,
                capacity: Arc::new(Semaphore::new(capacity)),
                capacity_limit,
                worker: Mutex::new(None),
                health: Arc::new(IoHealth::default()),
                shutdown: tokio::sync::Mutex::new(()),
            }),
        };
        Self { domain, receiver }
    }

    /// Releases one accepted preparation that did not start.
    fn release_prepare(&self) {
        self.receiver
            .try_recv()
            .expect("one preparation must be queued")
            .release_unstarted_prepare();
    }
}

/// Polls one future exactly once and requires a pending result.
async fn poll_pending<F>(mut future: Pin<&mut F>)
where
    F: Future + ?Sized,
{
    poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
}

/// Creates ready managed I/O with a manually observed removal queue.
fn observed_managed_io() -> (ManagedAttemptIo, mpsc::Receiver<IoJob>, Arc<Semaphore>) {
    let (sender, receiver) = mpsc::channel();
    let queue = Arc::new(IoQueue {
        sender: Mutex::new(Some(sender)),
    });
    let capacity = Arc::new(Semaphore::new(1));
    let permit = Arc::clone(&capacity)
        .try_acquire_owned()
        .expect("test I/O capacity must be available");
    let health = Arc::new(IoHealth::default());
    let managed = ManagedAttemptIo {
        state: ManagedIoState::Ready(ManagedAttemptIoInner::new(
            AttemptIo::for_test(),
            permit,
            Arc::clone(&health),
        )),
        queue,
        health,
        _test_worker: None,
    };
    (managed, receiver, capacity)
}

#[test]
fn admission_rejects_invalid_capacity() {
    let zero = match IoDomain::start(0) {
        Ok(_) => panic!("zero capacity must be rejected"),
        Err(error) => error,
    };
    assert_eq!(zero.class(), ContainerErrorClass::Permanent);

    let excessive = match IoDomain::start(Semaphore::MAX_PERMITS + 1) {
        Ok(_) => panic!("capacity above the semaphore limit must be rejected"),
        Err(error) => error,
    };
    assert_eq!(excessive.class(), ContainerErrorClass::Permanent);
}

#[tokio::test]
async fn maximum_supported_capacity_constructs_without_queue_slot_allocation() {
    let maximum = Semaphore::MAX_PERMITS
        .min(usize::try_from(u32::MAX).expect("supported targets can represent u32 capacity"));
    let domain = IoDomain::start(maximum).expect("maximum-capacity I/O domain must start");

    assert_eq!(domain.inner.capacity.available_permits(), maximum);
    domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("idle maximum-capacity I/O domain must shut down");
}

#[tokio::test]
async fn admission_uses_the_exact_configured_capacity() {
    let observed = ObservedDomain::new(3);
    let mut owners = Vec::new();
    for index in 0..3 {
        owners.push(
            observed
                .domain
                .try_prepare(PathBuf::from("queued"), format!("queued-{index}"))
                .expect("every configured I/O slot must be usable"),
        );
    }

    let error = match observed
        .domain
        .try_prepare(PathBuf::from("full"), "full".to_owned())
    {
        Ok(_) => panic!("one owner above the configured capacity must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.reason(), "containerd I/O admission is full");

    observed.release_prepare();
    let replacement = observed
        .domain
        .try_prepare(PathBuf::from("replacement"), "replacement".to_owned())
        .expect("releasing one owner must free exactly one I/O slot");

    for _ in 0..3 {
        observed.release_prepare();
    }
    drop(owners);
    drop(replacement);
    assert_eq!(observed.domain.inner.capacity.available_permits(), 3);
    observed
        .domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect("released exact-capacity domain must shut down");
}

#[tokio::test]
async fn admission_is_bounded_and_independent_per_domain() {
    let first = ObservedDomain::new(1);
    let second = ObservedDomain::new(1);

    let first_owner = first
        .domain
        .try_prepare(PathBuf::from("first"), "first".to_owned())
        .expect("first domain must accept one owner");
    let full = match first
        .domain
        .try_prepare(PathBuf::from("full"), "full".to_owned())
    {
        Ok(_) => panic!("first domain must reject a second owner"),
        Err(error) => error,
    };
    assert_eq!(full.class(), ContainerErrorClass::Retryable);
    assert_eq!(full.reason(), "containerd I/O admission is full");

    let second_owner = second
        .domain
        .try_prepare(PathBuf::from("second"), "second".to_owned())
        .expect("another domain must retain independent capacity");

    first.release_prepare();
    second.release_prepare();
    drop(first_owner);
    drop(second_owner);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    first.domain.shutdown_until(deadline).await.unwrap();
    second.domain.shutdown_until(deadline).await.unwrap();
}

#[tokio::test]
async fn preparation_join_retains_receiver_across_cancellation() {
    let (sender, receiver) = oneshot::channel();
    let mut preparation = IoPreparation {
        owner: super::PreparationOwner::Running(receiver),
    };

    let mut first_join = Box::pin(preparation.join());
    poll_pending(first_join.as_mut()).await;
    drop(first_join);
    sender
        .send(PrepareResult::Failed(ContainerEngineError::retryable(
            "test preparation failed",
        )))
        .unwrap_or_else(|_| panic!("preparation receiver must remain owned"));

    let error = match preparation.join().await {
        Ok(_) => panic!("prepared result must retain its failure"),
        Err(error) => error,
    };
    assert_eq!(error.reason(), "test preparation failed");
    assert!(!preparation.is_lost());
}

#[test]
fn dropped_buffered_preparation_removes_the_ready_owner() {
    let (managed, receiver, capacity) = observed_managed_io();
    let (sender, result) = oneshot::channel();
    let preparation = IoPreparation {
        owner: super::PreparationOwner::Running(result),
    };
    assert!(sender.send(PrepareResult::Ready(managed)).is_ok());

    drop(preparation);
    let mut job = receiver
        .try_recv()
        .expect("dropped buffered preparation must queue removal");
    assert_eq!(job.run(), WorkerProgress::Continue);
    job.disarm();
    assert_eq!(capacity.available_permits(), 1);
}

#[tokio::test]
async fn managed_cleanup_retains_receiver_across_cancellation() {
    let (mut managed, receiver, capacity) = observed_managed_io();

    let mut first_cleanup = Box::pin(managed.cleanup());
    poll_pending(first_cleanup.as_mut()).await;
    drop(first_cleanup);

    let mut job = receiver.try_recv().expect("cleanup must queue one removal");
    assert_eq!(job.run(), WorkerProgress::Continue);
    job.disarm();

    managed.cleanup().await.unwrap();
    assert!(matches!(managed.state, ManagedIoState::Released));
    assert!(!managed.is_lost());
    assert_eq!(capacity.available_permits(), 1);
}

#[tokio::test]
async fn failed_removal_restores_ready_ownership_for_retry() {
    let (mut managed, receiver, capacity) = observed_managed_io();
    let health = Arc::clone(&managed.health);
    let inner = match std::mem::replace(&mut managed.state, ManagedIoState::Lost) {
        ManagedIoState::Ready(inner) => inner,
        ManagedIoState::Removing(_) | ManagedIoState::Released | ManagedIoState::Lost => {
            panic!("test managed I/O must start ready")
        }
    };
    let (sender, receiver_result) = oneshot::channel();
    managed.state = ManagedIoState::Removing(RemovalOwner {
        receiver: receiver_result,
    });
    assert!(
        sender
            .send(RemoveResult::Retained {
                inner,
                error: std::io::Error::other("retry removal"),
            })
            .is_ok()
    );
    let error = managed.cleanup().await.unwrap_err();
    assert_eq!(error.reason(), "cannot clean up containerd output pipes");
    assert!(matches!(managed.state, ManagedIoState::Ready(_)));
    assert!(
        !health
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire)
    );

    let mut retry = Box::pin(managed.cleanup());
    poll_pending(retry.as_mut()).await;
    drop(retry);
    let mut job = receiver.try_recv().expect("retry must queue removal");
    assert_eq!(job.run(), WorkerProgress::Continue);
    job.disarm();
    managed.cleanup().await.unwrap();
    assert_eq!(capacity.available_permits(), 1);
}

#[tokio::test]
async fn shutdown_is_terminal_idempotent_and_cancellation_safe() {
    let observed = ObservedDomain::new(1);
    let preparation = observed
        .domain
        .try_prepare(PathBuf::from("held"), "held".to_owned())
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);

    let mut first_shutdown = Box::pin(observed.domain.shutdown_until(deadline));
    poll_pending(first_shutdown.as_mut()).await;
    drop(first_shutdown);
    let closed = match observed
        .domain
        .try_prepare(PathBuf::from("late"), "late".to_owned())
    {
        Ok(_) => panic!("terminal shutdown must keep admission closed"),
        Err(error) => error,
    };
    assert_eq!(closed.reason(), "containerd I/O admission is closed");

    observed.release_prepare();
    drop(preparation);
    observed.domain.shutdown_until(deadline).await.unwrap();
    observed.domain.shutdown_until(deadline).await.unwrap();
}

#[tokio::test]
async fn shutdown_uses_the_supplied_deadline() {
    let observed = ObservedDomain::new(1);
    let preparation = observed
        .domain
        .try_prepare(PathBuf::from("held"), "held".to_owned())
        .unwrap();

    let error = observed
        .domain
        .shutdown_until(tokio::time::Instant::now())
        .await
        .expect_err("held ownership must exceed an expired deadline");
    assert_eq!(error.class(), ContainerErrorClass::Retryable);
    assert_eq!(error.reason(), "containerd I/O shutdown deadline exceeded");

    observed.release_prepare();
    drop(preparation);
    observed
        .domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn known_quarantine_is_not_masked_by_a_blocked_worker() {
    let observed = ObservedDomain::new(1);
    let (release, blocked) = mpsc::channel();
    let worker = thread::spawn(move || {
        blocked.recv().expect("test worker release must arrive");
    });
    *observed
        .domain
        .inner
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(worker);
    observed
        .domain
        .inner
        .health
        .quarantined
        .store(true, std::sync::atomic::Ordering::Release);

    let error = observed
        .domain
        .shutdown_until(tokio::time::Instant::now())
        .await
        .expect_err("known quarantine must remain permanent");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd I/O ownership is quarantined");
    assert!(
        observed
            .domain
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    );

    release.send(()).unwrap();
    while !observed
        .domain
        .inner
        .worker
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .expect("blocked worker handle must remain retained")
        .is_finished()
    {
        tokio::task::yield_now().await;
    }
    let error = observed
        .domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect_err("terminal quarantine must remain visible");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert!(
        observed
            .domain
            .inner
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    );
}

#[test]
fn dropped_prepared_owner_hands_off_removal() {
    let (managed, receiver, capacity) = observed_managed_io();
    drop(managed);

    let mut job = receiver.try_recv().expect("drop must queue one removal");
    assert_eq!(job.run(), WorkerProgress::Continue);
    job.disarm();
    assert_eq!(capacity.available_permits(), 1);
}

#[tokio::test]
async fn lost_worker_job_retains_capacity_and_fails_shutdown() {
    let observed = ObservedDomain::new(1);
    let permit = Arc::clone(&observed.domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let (result, _receiver) = oneshot::channel();
    let job = IoJob::remove(
        ManagedAttemptIoInner::new(
            AttemptIo::for_test(),
            permit,
            Arc::clone(&observed.domain.inner.health),
        ),
        result,
        Arc::clone(&observed.domain.inner.health),
    );

    drop(job);
    assert!(
        observed
            .domain
            .inner
            .health
            .failed
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert_eq!(observed.domain.inner.capacity.available_permits(), 0);
    let error = observed
        .domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect_err("lost ownership must fail shutdown");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd I/O ownership is quarantined");
}

#[tokio::test]
async fn buffered_failed_removal_marks_quarantine_when_owner_drops() {
    let observed = ObservedDomain::new(1);
    let permit = Arc::clone(&observed.domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let health = Arc::clone(&observed.domain.inner.health);
    let inner = ManagedAttemptIoInner::new(AttemptIo::for_test(), permit, Arc::clone(&health));
    let (result, receiver) = oneshot::channel();
    let managed = ManagedAttemptIo {
        state: ManagedIoState::Removing(RemovalOwner { receiver }),
        queue: Arc::clone(&observed.domain.inner.queue),
        health: Arc::clone(&health),
        _test_worker: None,
    };
    assert!(
        result
            .send(RemoveResult::Retained {
                inner,
                error: std::io::Error::other("test removal failure"),
            })
            .is_ok()
    );

    drop(managed);
    assert!(health.failed.load(std::sync::atomic::Ordering::Acquire));
    assert!(
        health
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert_eq!(observed.domain.inner.capacity.available_permits(), 0);
    let error = observed
        .domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect_err("buffered unresolved removal must fail shutdown");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd I/O ownership is quarantined");
}

#[test]
fn partial_prepare_failure_releases_capacity_after_successful_rollback() {
    let observed = ObservedDomain::new(1);
    let permit = Arc::clone(&observed.domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let health = Arc::clone(&observed.domain.inner.health);
    let (result, receiver) = oneshot::channel();
    let mut job = PrepareJob {
        root: PathBuf::from("unused"),
        attempt_id: "partial".to_owned(),
        permit: Some(permit),
        queue: Arc::clone(&observed.domain.inner.queue),
        result: Some(result),
    };

    assert_eq!(
        finish_failed_partial_prepare(
            &mut job,
            &health,
            std::io::Error::other("injected preparation failure"),
            AttemptIo::for_test(),
        ),
        WorkerProgress::Continue,
    );
    assert!(matches!(
        receiver.blocking_recv(),
        Ok(PrepareResult::Failed(_))
    ));
    assert_eq!(observed.domain.inner.capacity.available_permits(), 1);
    assert!(!super::health_failed(&health));
}

#[test]
fn partial_prepare_rollback_failure_quarantines_ownership() {
    let observed = ObservedDomain::new(1);
    let permit = Arc::clone(&observed.domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let health = Arc::clone(&observed.domain.inner.health);
    let (result, receiver) = oneshot::channel();
    let mut job = PrepareJob {
        root: PathBuf::from("unused"),
        attempt_id: "partial".to_owned(),
        permit: Some(permit),
        queue: Arc::clone(&observed.domain.inner.queue),
        result: Some(result),
    };

    assert_eq!(
        finish_failed_partial_prepare(
            &mut job,
            &health,
            std::io::Error::other("injected preparation failure"),
            AttemptIo::for_test_with_cleanup_error(std::io::ErrorKind::PermissionDenied),
        ),
        WorkerProgress::Stop,
    );
    assert!(matches!(
        receiver.blocking_recv(),
        Ok(PrepareResult::Quarantined(_))
    ));
    assert_eq!(observed.domain.inner.capacity.available_permits(), 0);
    assert!(
        health
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire)
    );
}

#[test]
fn orphan_prepared_owner_cleanup_failure_quarantines_ownership() {
    let observed = ObservedDomain::new(1);
    let permit = Arc::clone(&observed.domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let health = Arc::clone(&observed.domain.inner.health);
    let mut managed = ManagedAttemptIo {
        state: ManagedIoState::Ready(ManagedAttemptIoInner::new(
            AttemptIo::for_test_with_cleanup_error(std::io::ErrorKind::PermissionDenied),
            permit,
            Arc::clone(&health),
        )),
        queue: Arc::clone(&observed.domain.inner.queue),
        health: Arc::clone(&health),
        _test_worker: None,
    };

    assert_eq!(
        remove_or_quarantine_managed(&mut managed),
        WorkerProgress::Stop,
    );
    assert_eq!(observed.domain.inner.capacity.available_permits(), 0);
    assert!(
        health
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire)
    );
}

#[tokio::test]
async fn worker_panic_quarantines_capacity_and_fails_closed() {
    let domain = IoDomain::start(1).unwrap();
    let permit = Arc::clone(&domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let job = IoJob {
        inner: Some(IoJobInner::Prepare(PrepareJob {
            root: PathBuf::from("/solti-io-domain-missing-test-root"),
            attempt_id: "panic".to_owned(),
            permit: Some(permit),
            queue: Arc::clone(&domain.inner.queue),
            result: None,
        })),
        health: Arc::clone(&domain.inner.health),
    };
    assert!(domain.inner.queue.try_send(job).is_ok());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while !domain
        .inner
        .health
        .panicked
        .load(std::sync::atomic::Ordering::Acquire)
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    assert_eq!(domain.inner.capacity.available_permits(), 0);
    let error = domain
        .shutdown_until(deadline)
        .await
        .expect_err("worker panic must fail shutdown");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd I/O worker panicked");
}

#[tokio::test]
async fn explicit_quarantine_retains_capacity_and_fails_shutdown() {
    let observed = ObservedDomain::new(1);
    let permit = Arc::clone(&observed.domain.inner.capacity)
        .try_acquire_owned()
        .unwrap();
    let (result, _receiver) = oneshot::channel();
    let job = IoJob::remove(
        ManagedAttemptIoInner::new(
            AttemptIo::for_test(),
            permit,
            Arc::clone(&observed.domain.inner.health),
        ),
        result,
        Arc::clone(&observed.domain.inner.health),
    );

    job.quarantine("test");
    assert!(
        observed
            .domain
            .inner
            .health
            .quarantined
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert_eq!(observed.domain.inner.capacity.available_permits(), 0);
    let error = observed
        .domain
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .expect_err("quarantined ownership must fail shutdown");
    assert_eq!(error.class(), ContainerErrorClass::Permanent);
    assert_eq!(error.reason(), "containerd I/O ownership is quarantined");
}
