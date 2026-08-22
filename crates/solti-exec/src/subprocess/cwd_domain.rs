//! Runner-owned blocking working-directory preparation.
//!
//! Task builds submit path resolution and descriptor pinning to one dedicated
//! thread. A semaphore bounds queued and active work. Cancelling or dropping a
//! build releases its receiver while the accepted job remains owned until the
//! worker finishes and drops its result.

use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread,
    time::Duration,
};

use solti_runner::BuildCancellation;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};

use super::{backend::PreparedSubprocessBackendConfig, boundary::PinnedCwd};

/// Failure returned while a task build prepares its working directory.
pub(super) enum CwdPinError {
    /// The requested directory violates the configured cwd policy.
    InvalidSpec(String),
    /// Build cancellation won while the operation was pending or running.
    Cancelled,
    /// The runner-owned worker cannot accept or finish the operation.
    Unavailable(String),
}

/// Bounded blocking path-resolution domain owned by one subprocess runner.
#[derive(Clone)]
pub(super) struct CwdDomain {
    inner: Arc<CwdDomainInner>,
}

struct CwdDomainInner {
    capacity: usize,
    admission: Arc<Semaphore>,
    sender: Mutex<Option<Sender<CwdJob>>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
    failed: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    shutdown_changed: Arc<Notify>,
    shutdown: tokio::sync::Mutex<()>,
}

struct CwdJob {
    operation: CwdOperation,
    result: oneshot::Sender<Result<Option<PinnedCwd>, String>>,
    _permit: OwnedSemaphorePermit,
}

enum CwdOperation {
    Pin {
        config: Arc<PreparedSubprocessBackendConfig>,
        cwd: Option<PathBuf>,
    },
    #[cfg(test)]
    Block {
        started: oneshot::Sender<()>,
        release: mpsc::Receiver<()>,
    },
}

impl CwdDomain {
    /// Starts one dedicated blocking worker with exact operation admission.
    pub(super) fn start(capacity: usize) -> io::Result<Self> {
        if capacity == 0 || u32::try_from(capacity).is_err() || capacity > Semaphore::MAX_PERMITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "subprocess cwd I/O capacity is outside the supported range",
            ));
        }

        // Queue nodes are allocated only for admitted work. The semaphore is
        // the exact bound, including the operation currently on the worker.
        let (sender, receiver) = mpsc::channel();
        let failed = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_failed = Arc::clone(&failed);
        let worker_stopped = Arc::clone(&stopped);
        let shutdown_changed = Arc::new(Notify::new());
        let worker_shutdown_changed = Arc::clone(&shutdown_changed);
        let worker = thread::Builder::new()
            .name("solti-exec-cwd".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| run_worker(receiver)));
                if let Err(payload) = outcome {
                    // A caught payload is untrusted. Leaking this exceptional
                    // value prevents a panicking destructor from aborting the
                    // process while the worker records its terminal failure.
                    std::mem::forget(payload);
                    worker_failed.store(true, Ordering::Release);
                }
                worker_stopped.store(true, Ordering::Release);
                worker_shutdown_changed.notify_waiters();
            })
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to start subprocess cwd I/O worker: {error}"),
                )
            })?;

        Ok(Self {
            inner: Arc::new(CwdDomainInner {
                capacity,
                admission: Arc::new(Semaphore::new(capacity)),
                sender: Mutex::new(Some(sender)),
                worker: Mutex::new(Some(worker)),
                failed,
                stopped,
                shutdown_changed,
                shutdown: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Resolves and pins one configured cwd outside Tokio runtime workers.
    pub(super) async fn pin(
        &self,
        config: Arc<PreparedSubprocessBackendConfig>,
        cwd: Option<PathBuf>,
        cancellation: &BuildCancellation,
    ) -> Result<Option<PinnedCwd>, CwdPinError> {
        self.submit(CwdOperation::Pin { config, cwd }, cancellation)
            .await
    }

    async fn submit(
        &self,
        operation: CwdOperation,
        cancellation: &BuildCancellation,
    ) -> Result<Option<PinnedCwd>, CwdPinError> {
        self.ensure_available()?;
        let permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(CwdPinError::Cancelled),
            permit = Arc::clone(&self.inner.admission).acquire_owned() => permit.map_err(|_| {
                CwdPinError::Unavailable("subprocess cwd I/O admission is closed".into())
            })?,
        };
        self.ensure_available()?;

        let (result, receiver) = oneshot::channel();
        let job = CwdJob {
            operation,
            result,
            _permit: permit,
        };
        let sender = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                CwdPinError::Unavailable("subprocess cwd I/O admission is closed".into())
            })?;
        if sender.send(job).is_err() {
            self.inner.failed.store(true, Ordering::Release);
            return Err(CwdPinError::Unavailable(
                "subprocess cwd I/O worker is unavailable".into(),
            ));
        }

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(CwdPinError::Cancelled),
            result = receiver => match result {
                Ok(Ok(pinned)) => Ok(pinned),
                Ok(Err(error)) => Err(CwdPinError::InvalidSpec(error)),
                Err(_) => {
                    self.inner.failed.store(true, Ordering::Release);
                    Err(CwdPinError::Unavailable(
                        "subprocess cwd I/O worker stopped before reporting its result".into(),
                    ))
                }
            }
        }
    }

    #[cfg(test)]
    async fn block_for_test(
        &self,
        started: oneshot::Sender<()>,
        release: mpsc::Receiver<()>,
        cancellation: &BuildCancellation,
    ) -> Result<Option<PinnedCwd>, CwdPinError> {
        self.submit(CwdOperation::Block { started, release }, cancellation)
            .await
    }

    /// Closes admission and joins the worker after every accepted operation.
    ///
    /// Cancellation leaves the domain closed and its worker handle retained so
    /// another call can continue the same shutdown.
    pub(super) async fn shutdown(&self, timeout: Duration) -> io::Result<()> {
        let _shutdown = self.inner.shutdown.lock().await;
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "subprocess cwd I/O shutdown timeout exceeds the supported range",
                )
            })?;
        self.inner.admission.close();
        self.inner
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();

        loop {
            // Register before inspecting state. A terminal worker transition
            // between this check and `.await` then wakes this exact waiter.
            let changed = self.inner.shutdown_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            if self.inner.admission.available_permits() == self.inner.capacity
                && self.inner.stopped.load(Ordering::Acquire)
            {
                // `stopped` is published after the receiver and every queued
                // operation have been dropped. Join the terminal epilogue
                // directly; an `is_finished` check here can miss the worker's
                // final notification just before the thread returns.
                join_stopped_worker(&self.inner.worker)?;
                return if self.inner.failed.load(Ordering::Acquire) {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "subprocess cwd I/O worker lost forward progress",
                    ))
                } else {
                    Ok(())
                };
            }
            if self.inner.failed.load(Ordering::Acquire)
                && self.inner.stopped.load(Ordering::Acquire)
            {
                join_stopped_worker(&self.inner.worker)?;
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "subprocess cwd I/O worker lost forward progress",
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "subprocess cwd I/O shutdown deadline exceeded",
                ));
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                // Re-check terminal state once at the deadline. Completion
                // keeps precedence when it races the timer.
                continue;
            }
        }
    }

    fn ensure_available(&self) -> Result<(), CwdPinError> {
        let sender_open = self
            .inner
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some();
        if !sender_open
            || self.inner.admission.is_closed()
            || self.inner.failed.load(Ordering::Acquire)
            || self.inner.stopped.load(Ordering::Acquire)
            || worker_finished(&self.inner.worker)
        {
            return Err(CwdPinError::Unavailable(
                "subprocess cwd I/O worker is unavailable".into(),
            ));
        }
        Ok(())
    }
}

impl Drop for CwdDomainInner {
    fn drop(&mut self) {
        self.admission.close();
        self.sender
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(worker);
    }
}

fn run_worker(receiver: mpsc::Receiver<CwdJob>) {
    for job in receiver {
        let result = match job.operation {
            CwdOperation::Pin { config, cwd } => config.pin_cwd(cwd.as_deref()),
            #[cfg(test)]
            CwdOperation::Block { started, release } => {
                let _ = started.send(());
                release
                    .recv()
                    .map(|()| None)
                    .map_err(|_| "test cwd worker release sender was dropped".into())
            }
        };
        let _ = job.result.send(result);
    }
}

fn worker_finished(worker: &Mutex<Option<thread::JoinHandle<()>>>) -> bool {
    worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_none_or(thread::JoinHandle::is_finished)
}

fn join_stopped_worker(worker: &Mutex<Option<thread::JoinHandle<()>>>) -> io::Result<()> {
    let worker = worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    match worker {
        Some(worker) => worker.join().map_err(|payload| {
            std::mem::forget(payload);
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "subprocess cwd I/O worker panicked",
            )
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_shutdown_admission_to_close(domain: &CwdDomain) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !domain.inner.admission.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cwd shutdown did not close admission");
    }

    #[tokio::test]
    async fn cancelled_operation_remains_owned_until_worker_finishes() {
        let domain = CwdDomain::start(1).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (cancel, cancellation) = BuildCancellation::pair();
        let operation_domain = domain.clone();
        let operation = tokio::spawn(async move {
            operation_domain
                .block_for_test(started_tx, release_rx, &cancellation)
                .await
        });
        started_rx.await.unwrap();

        cancel.cancel();
        assert!(matches!(
            operation.await.unwrap(),
            Err(CwdPinError::Cancelled)
        ));
        let timed_out = domain.shutdown(Duration::from_millis(1)).await.unwrap_err();
        assert_eq!(timed_out.kind(), io::ErrorKind::TimedOut);

        release_tx.send(()).unwrap();
        domain.shutdown(Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn maximum_supported_capacity_does_not_allocate_queue_slots() {
        let maximum = usize::try_from(u32::MAX)
            .unwrap_or(usize::MAX)
            .min(Semaphore::MAX_PERMITS);
        let domain = CwdDomain::start(maximum).unwrap();

        domain.shutdown(Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_wakes_when_the_active_operation_releases_and_worker_stops() {
        let domain = CwdDomain::start(1).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (_cancel, cancellation) = BuildCancellation::pair();
        let operation_domain = domain.clone();
        let operation = tokio::spawn(async move {
            operation_domain
                .block_for_test(started_tx, release_rx, &cancellation)
                .await
        });
        started_rx.await.unwrap();
        let shutdown_domain = domain.clone();
        let shutdown =
            tokio::spawn(async move { shutdown_domain.shutdown(Duration::from_secs(2)).await });

        wait_for_shutdown_admission_to_close(&domain).await;
        release_tx.send(()).unwrap();

        assert!(matches!(operation.await.unwrap(), Ok(None)));
        shutdown.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_can_be_canceled_and_retried() {
        let domain = CwdDomain::start(1).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (_cancel, cancellation) = BuildCancellation::pair();
        let operation_domain = domain.clone();
        let operation = tokio::spawn(async move {
            operation_domain
                .block_for_test(started_tx, release_rx, &cancellation)
                .await
        });
        started_rx.await.unwrap();
        let shutdown_domain = domain.clone();
        let shutdown =
            tokio::spawn(async move { shutdown_domain.shutdown(Duration::from_secs(30)).await });

        wait_for_shutdown_admission_to_close(&domain).await;
        shutdown.abort();
        assert!(shutdown.await.unwrap_err().is_cancelled());

        release_tx.send(()).unwrap();
        assert!(matches!(operation.await.unwrap(), Ok(None)));
        domain.shutdown(Duration::from_secs(2)).await.unwrap();
    }

    #[tokio::test]
    async fn repeated_shutdown_of_drained_domain_succeeds_without_waiting() {
        let domain = CwdDomain::start(1).unwrap();

        let started = std::time::Instant::now();
        domain.shutdown(Duration::from_secs(2)).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "drained cwd domain waited until the shutdown deadline"
        );
        domain.shutdown(Duration::ZERO).await.unwrap();
    }

    #[tokio::test]
    async fn overflowing_shutdown_timeout_is_rejected_without_closing_admission() {
        let domain = CwdDomain::start(1).unwrap();

        let error = domain.shutdown(Duration::MAX).await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!domain.inner.admission.is_closed());
        domain.shutdown(Duration::from_secs(2)).await.unwrap();
    }
}
