//! # Subprocess attempt domain
//!
//! [`ActiveProcessDomain`] owns one child and its host process domain.
//! It keeps the Unix process-group identity reserved until termination.
//! A dropped domain transfers both resources to one Tokio-independent finalizer.
//! Drop does not wait for process exit.

use std::{
    io,
    path::Path,
    process::ExitStatus,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SendError, Sender, channel},
    },
    time::{Duration, Instant},
};

use tracing::warn;

use crate::host::{AttemptProcessDomain, DomainTermination, PreparedHostProcessAttempt};
use crate::subprocess::child::{ChildOutput, ProcessChild};

/// Child process and termination boundary for one active attempt.
///
/// Drop requests termination before scheduling reap and cleanup.
pub(super) struct ActiveProcessDomain {
    child: Option<ProcessChild>,
    host: Option<AttemptProcessDomain>,
    drop_finalizer: Option<DropFinalizerReservation>,
    #[cfg(unix)]
    group: ProcessGroupState,
    #[cfg(not(unix))]
    observed_status: Option<ExitStatus>,
    leader: LeaderState,
    terminated: bool,
    run_id: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaderState {
    Running,
    ExitedObserved,
    KillRequested,
    Reaped,
    WaitOwnershipLost,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessGroupState {
    Armed(libc::pid_t),
    Handled,
    Released,
}

#[cfg(unix)]
type DroppedProcessGroup = ProcessGroupState;
#[cfg(not(unix))]
type DroppedProcessGroup = ();

impl ActiveProcessDomain {
    /// Arms supervision for a freshly spawned child.
    pub(super) fn new<C>(
        child: C,
        host: AttemptProcessDomain,
        run_id: Arc<str>,
        drop_finalizer: DropFinalizerReservation,
    ) -> Self
    where
        C: Into<ProcessChild>,
    {
        let child = child.into();
        #[cfg(unix)]
        let group = child.id().map_or(ProcessGroupState::Released, |pid| {
            ProcessGroupState::Armed(pid as libc::pid_t)
        });

        Self {
            child: Some(child),
            host: Some(host),
            drop_finalizer: Some(drop_finalizer),
            #[cfg(unix)]
            group,
            #[cfg(not(unix))]
            observed_status: None,
            leader: LeaderState::Running,
            terminated: false,
            run_id,
        }
    }

    /// Takes the child's stdout pipe.
    pub(super) fn take_stdout(&mut self) -> Option<ChildOutput> {
        self.child
            .as_mut()
            .expect("active process domain must own its child")
            .take_stdout()
    }

    /// Takes the child's stderr pipe.
    pub(super) fn take_stderr(&mut self) -> Option<ChildOutput> {
        self.child
            .as_mut()
            .expect("active process domain must own its child")
            .take_stderr()
    }

    /// Returns the attempt cgroup path when one is configured.
    pub(super) fn cgroup_path(&self) -> Option<&Path> {
        self.host
            .as_ref()
            .and_then(AttemptProcessDomain::cgroup_path)
    }

    /// Observes leader exit without releasing its Unix process identity.
    #[cfg(unix)]
    pub(super) async fn observe_exit(&mut self) -> io::Result<()> {
        use tokio::signal::unix::{SignalKind, signal};

        let pid = self
            .child
            .as_ref()
            .and_then(ProcessChild::id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "subprocess has no process id")
            })?;
        let pid = pid as libc::pid_t;
        let mut sigchld = signal(SignalKind::child())?;

        loop {
            match exited_without_reaping(pid) {
                Ok(true) => {
                    self.leader = LeaderState::ExitedObserved;
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    if error.raw_os_error() == Some(libc::ECHILD) {
                        self.leader = LeaderState::WaitOwnershipLost;
                        self.group = ProcessGroupState::Released;
                    }
                    return Err(error);
                }
            }
            if sigchld.recv().await.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "SIGCHLD listener closed before subprocess exit",
                ));
            }
        }
    }

    /// Waits for leader exit on platforms without `waitid(WNOWAIT)`.
    #[cfg(not(unix))]
    pub(super) async fn observe_exit(&mut self) -> io::Result<()> {
        self.observed_status = Some(
            self.child
                .as_mut()
                .expect("active process domain must own its child")
                .wait()
                .await?,
        );
        self.leader = LeaderState::ExitedObserved;
        Ok(())
    }

    /// Terminates the strongest available attempt boundary.
    ///
    /// Cgroup and Unix process-group termination are applied together.
    /// The process-group identity is consumed before the leader can be reaped.
    pub(super) fn terminate(&mut self) -> io::Result<()> {
        if self.terminated {
            return Ok(());
        }

        let tree = match self.host.as_mut() {
            Some(host) => host.terminate_tree(),
            None => Ok(DomainTermination::Unavailable),
        };
        let group = self.terminate_group();
        let leader = self.terminate_leader();
        let result = finish_termination(tree, group, leader);
        if result.is_ok() {
            self.terminated = true;
        }
        result
    }

    /// Returns `true` when the leader can be reaped without blocking on normal execution.
    pub(super) fn leader_can_be_reaped(&self) -> bool {
        let leader_ready = matches!(
            self.leader,
            LeaderState::ExitedObserved | LeaderState::KillRequested
        );
        #[cfg(unix)]
        {
            leader_ready && !matches!(self.group, ProcessGroupState::Armed(_))
        }
        #[cfg(not(unix))]
        {
            leader_ready
        }
    }

    /// Reaps the leader after its termination boundary has been handled.
    pub(super) async fn reap(&mut self) -> io::Result<ExitStatus> {
        debug_assert!(
            self.leader_can_be_reaped(),
            "leader exit must be observed or requested before reap"
        );
        #[cfg(unix)]
        {
            self.group = ProcessGroupState::Released;
        }
        #[cfg(not(unix))]
        if let Some(status) = self.observed_status.take() {
            self.leader = LeaderState::Reaped;
            return Ok(status);
        }
        let result = self
            .child
            .as_mut()
            .expect("active process domain must own its child")
            .wait()
            .await;
        self.leader = match result {
            Ok(_) => LeaderState::Reaped,
            Err(_) => LeaderState::WaitOwnershipLost,
        };
        result
    }

    /// Removes attempt resources after the complete domain has stopped.
    pub(super) async fn cleanup(&mut self) -> io::Result<()> {
        let Some(host) = self.host.as_mut() else {
            return Ok(());
        };
        let result = cleanup_host_domain(host).await;
        if result.is_ok() {
            self.host.take();
        }
        result
    }

    #[cfg(unix)]
    fn terminate_group(&mut self) -> io::Result<()> {
        terminate_process_group(&mut self.group, self.leader)
    }

    #[cfg(not(unix))]
    fn terminate_group(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn terminate_leader(&mut self) -> io::Result<()> {
        match self.leader {
            LeaderState::ExitedObserved | LeaderState::KillRequested | LeaderState::Reaped => {
                Ok(())
            }
            LeaderState::WaitOwnershipLost => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "subprocess wait ownership was lost",
            )),
            LeaderState::Running => match self
                .child
                .as_mut()
                .expect("active process domain must own its child")
                .start_kill()
            {
                Ok(()) => {
                    self.leader = LeaderState::KillRequested;
                    Ok(())
                }
                Err(error) => Err(error),
            },
        }
    }

    #[cfg(all(test, unix))]
    fn process_group_id(&self) -> Option<libc::pid_t> {
        match self.group {
            ProcessGroupState::Armed(pgid) => Some(pgid),
            ProcessGroupState::Handled | ProcessGroupState::Released => None,
        }
    }
}

#[cfg(unix)]
fn terminate_process_group(group: &mut ProcessGroupState, leader: LeaderState) -> io::Result<()> {
    #[cfg(not(target_os = "macos"))]
    let _ = leader;

    let pgid = match *group {
        ProcessGroupState::Handled | ProcessGroupState::Released => return Ok(()),
        ProcessGroupState::Armed(pgid) => pgid,
    };

    // SAFETY: `kill` accepts a scalar process-group identifier.
    let result = unsafe { libc::kill(-pgid, libc::SIGKILL) };
    if result == 0 {
        *group = ProcessGroupState::Handled;
        return Ok(());
    }

    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        *group = ProcessGroupState::Handled;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    if matches!(
        leader,
        LeaderState::ExitedObserved | LeaderState::KillRequested
    ) && error.raw_os_error() == Some(libc::EPERM)
    {
        match macos_group_contains_only_leader(pgid) {
            Ok(true) => {
                *group = ProcessGroupState::Handled;
                return Ok(());
            }
            Ok(false) => {}
            Err(inspection) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "process-group termination failed: {error}; group inspection failed: {inspection}"
                    ),
                ));
            }
        }
    }

    Err(error)
}

#[cfg(target_os = "macos")]
fn macos_group_contains_only_leader(pgid: libc::pid_t) -> io::Result<bool> {
    const PROC_PGRP_ONLY: libc::c_uint = 2;
    const EXTRA_SLOTS: usize = 16;

    unsafe extern "C" {
        fn proc_listpids(
            kind: libc::c_uint,
            type_info: libc::c_uint,
            buffer: *mut libc::c_void,
            buffer_size: libc::c_int,
        ) -> libc::c_int;
    }

    // SAFETY: a null buffer with size zero queries the required capacity.
    let required = unsafe {
        proc_listpids(
            PROC_PGRP_ONLY,
            pgid as libc::c_uint,
            std::ptr::null_mut(),
            0,
        )
    };
    if required < 0 {
        return Err(io::Error::last_os_error());
    }

    let slots = (required as usize)
        .div_ceil(std::mem::size_of::<libc::pid_t>())
        .saturating_add(EXTRA_SLOTS);
    let mut pids = vec![0 as libc::pid_t; slots];
    let buffer_size = pids
        .len()
        .checked_mul(std::mem::size_of::<libc::pid_t>())
        .and_then(|size| libc::c_int::try_from(size).ok())
        .ok_or_else(|| io::Error::other("process-group inspection buffer is too large"))?;

    // SAFETY: `pids` is writable for exactly `buffer_size` bytes.
    let returned = unsafe {
        proc_listpids(
            PROC_PGRP_ONLY,
            pgid as libc::c_uint,
            pids.as_mut_ptr().cast(),
            buffer_size,
        )
    };
    if returned < 0 {
        return Err(io::Error::last_os_error());
    }
    if returned == buffer_size {
        return Ok(false);
    }

    let count = returned as usize / size_of::<libc::pid_t>();
    let mut saw_leader = false;
    for pid in pids.into_iter().take(count).filter(|pid| *pid != 0) {
        if pid != pgid {
            return Ok(false);
        }
        saw_leader = true;
    }
    Ok(saw_leader)
}

impl Drop for ActiveProcessDomain {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                warn!(
                    event = "subprocess.drop_termination_failed",
                    run_id = %self.run_id,
                    error = %error,
                    "failed to terminate subprocess domain on drop",
                );
            }));
        }

        let child = self.child.take();
        let host = self.host.take();
        #[cfg(unix)]
        let group = std::mem::replace(&mut self.group, ProcessGroupState::Released);
        #[cfg(not(unix))]
        let group = ();
        if self.leader == LeaderState::Reaped && host.is_none() {
            drop(child);
            return;
        }

        self.drop_finalizer
            .take()
            .expect("active process domain owns finalizer admission")
            .submit(DroppedProcessDomain::new(child, host, group, self.leader));
    }
}

/// Observable state of one subprocess runner's bounded finalizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubprocessFinalizerStatus {
    accepting: bool,
    healthy: bool,
    owned: usize,
    capacity: usize,
    quarantined: usize,
}

impl SubprocessFinalizerStatus {
    /// Returns whether new attempt ownership may be reserved.
    pub fn accepting(self) -> bool {
        self.accepting
    }

    /// Returns whether the finalizer has preserved forward progress.
    pub fn healthy(self) -> bool {
        self.healthy
    }

    /// Returns active, queued, finalizing, and quarantined ownership.
    pub fn owned(self) -> usize {
        self.owned
    }

    /// Returns the configured ownership limit.
    pub fn capacity(self) -> usize {
        self.capacity
    }

    /// Returns ownership retained after terminal cleanup failure.
    pub fn quarantined(self) -> usize {
        self.quarantined
    }
}

/// Bounded finalizer domain owned by one subprocess runner.
#[derive(Clone)]
pub(super) struct DropFinalizerDomain {
    inner: Arc<DropFinalizerDomainInner>,
}

struct DropFinalizerDomainInner {
    state: Arc<DropFinalizerState>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct DropFinalizerAdmission {
    accepting: bool,
    sender: Option<Sender<DroppedProcessJob>>,
}

struct DropFinalizerState {
    admission: Mutex<DropFinalizerAdmission>,
    active: Mutex<Vec<DroppedProcessJob>>,
    quarantine: Mutex<Vec<DroppedProcessJob>>,
    owned: AtomicUsize,
    capacity: usize,
    healthy: AtomicBool,
    quarantined: AtomicUsize,
    worker_stopped: AtomicBool,
    #[cfg(test)]
    panic_worker_once: AtomicBool,
    #[cfg(test)]
    input_closed_active_sleeps: AtomicUsize,
    #[cfg(test)]
    unhealthy_before_join_once: AtomicBool,
}

impl DropFinalizerState {
    fn close_admission(&self) {
        let mut admission = lock_admission(self);
        admission.accepting = false;
        admission.sender.take();
    }

    fn status(&self) -> SubprocessFinalizerStatus {
        let accepting = lock_admission(self).accepting;
        SubprocessFinalizerStatus {
            accepting,
            healthy: self.healthy.load(Ordering::Acquire),
            owned: self.owned.load(Ordering::Acquire),
            capacity: self.capacity,
            quarantined: self.quarantined.load(Ordering::Acquire),
        }
    }
}

impl Drop for DropFinalizerState {
    fn drop(&mut self) {
        let active = self
            .active
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let quarantine = self
            .quarantine
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for job in active.drain(..).chain(quarantine.drain(..)) {
            std::mem::forget(job);
        }
    }
}

impl Drop for DropFinalizerDomainInner {
    fn drop(&mut self) {
        self.state.close_admission();
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(worker);
    }
}

impl DropFinalizerDomain {
    /// Starts a Tokio-independent finalizer with bounded ownership admission.
    pub(super) fn start(capacity: usize) -> io::Result<Self> {
        if capacity == 0 || u32::try_from(capacity).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "subprocess cleanup capacity is outside the supported range",
            ));
        }
        let (sender, receiver) = channel();
        let state = Arc::new(DropFinalizerState {
            admission: Mutex::new(DropFinalizerAdmission {
                accepting: true,
                sender: Some(sender),
            }),
            active: Mutex::new(Vec::new()),
            quarantine: Mutex::new(Vec::new()),
            owned: AtomicUsize::new(0),
            capacity,
            healthy: AtomicBool::new(true),
            quarantined: AtomicUsize::new(0),
            worker_stopped: AtomicBool::new(false),
            #[cfg(test)]
            panic_worker_once: AtomicBool::new(false),
            #[cfg(test)]
            input_closed_active_sleeps: AtomicUsize::new(0),
            #[cfg(test)]
            unhealthy_before_join_once: AtomicBool::new(false),
        });
        let worker_state = Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("solti-exec-reaper".into())
            .spawn(move || run_drop_finalizer(worker_state, &receiver))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to start subprocess drop finalizer: {error}"),
                )
            })?;
        Ok(Self {
            inner: Arc::new(DropFinalizerDomainInner {
                state,
                worker: Mutex::new(Some(worker)),
            }),
        })
    }

    /// Reserves one active or deferred ownership slot before resource creation.
    pub(super) fn try_reserve(&self) -> io::Result<DropFinalizerReservation> {
        let state = &self.inner.state;
        let mut admission = lock_admission(state);
        if !admission.accepting {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "subprocess cleanup admission is closed",
            ));
        }
        if !state.healthy.load(Ordering::Acquire)
            || state.worker_stopped.load(Ordering::Acquire)
            || self
                .inner
                .worker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .is_none_or(std::thread::JoinHandle::is_finished)
        {
            state.healthy.store(false, Ordering::Release);
            admission.accepting = false;
            admission.sender.take();
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "subprocess cleanup worker is unavailable",
            ));
        }
        if state.owned.load(Ordering::Acquire) >= state.capacity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "subprocess cleanup admission is full",
            ));
        }
        let sender = admission
            .sender
            .as_ref()
            .expect("accepting finalizer admission owns its sender")
            .clone();
        state.owned.fetch_add(1, Ordering::AcqRel);
        Ok(DropFinalizerReservation {
            sender: Some(sender),
            permit: Some(DropFinalizerPermit {
                state: Arc::clone(state),
            }),
        })
    }

    /// Returns current admission, health, and ownership counters.
    pub(super) fn status(&self) -> SubprocessFinalizerStatus {
        self.inner.state.status()
    }

    /// Closes admission and waits for accepted ownership and the worker.
    ///
    /// Cancellation leaves admission closed and the worker handle retained.
    pub(super) async fn shutdown(&self, timeout: Duration) -> io::Result<()> {
        self.inner.state.close_admission();
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        loop {
            let status = self.status();
            if status.quarantined > 0 {
                return Err(io::Error::other(
                    "subprocess cleanup ownership is quarantined",
                ));
            }
            if status.owned == 0
                && self.inner.state.worker_stopped.load(Ordering::Acquire)
                && worker_is_finished(&self.inner.worker)
            {
                #[cfg(test)]
                if self
                    .inner
                    .state
                    .unhealthy_before_join_once
                    .swap(false, Ordering::AcqRel)
                {
                    self.inner.state.healthy.store(false, Ordering::Release);
                }
                join_finished_worker(&self.inner.worker)?;
                return if self.inner.state.healthy.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "subprocess cleanup worker lost forward progress",
                    ))
                };
            }
            if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "subprocess cleanup shutdown deadline exceeded",
                ));
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

fn worker_is_finished(worker: &Mutex<Option<std::thread::JoinHandle<()>>>) -> bool {
    worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .is_none_or(std::thread::JoinHandle::is_finished)
}

fn join_finished_worker(worker: &Mutex<Option<std::thread::JoinHandle<()>>>) -> io::Result<()> {
    let worker = worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    match worker {
        Some(worker) => worker.join().map_err(|payload| {
            std::mem::forget(payload);
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "subprocess cleanup worker panicked",
            )
        }),
        None => Ok(()),
    }
}

/// One ownership unit reserved before attempt resources are created.
pub(super) struct DropFinalizerReservation {
    sender: Option<Sender<DroppedProcessJob>>,
    permit: Option<DropFinalizerPermit>,
}

impl DropFinalizerReservation {
    /// Hands resources from an attempt that never spawned to the finalizer.
    pub(super) fn submit_unspawned(self, host: AttemptProcessDomain) {
        self.submit(unspawned_process_domain(host));
    }

    /// Retains one terminal ownership unit when no safe cleanup identity exists.
    pub(super) fn quarantine_unrecoverable(self) {
        let state = Arc::clone(
            &self
                .permit
                .as_ref()
                .expect("finalizer reservation owns its permit")
                .state,
        );
        state.healthy.store(false, Ordering::Release);
        state.close_admission();
        #[cfg(unix)]
        let group = ProcessGroupState::Released;
        #[cfg(not(unix))]
        let group = ();
        let mut domain = DroppedProcessDomain::new(None, None, group, LeaderState::Reaped);
        domain.quarantined = true;
        self.submit(domain);
    }

    fn submit(mut self, domain: DroppedProcessDomain) {
        let state = Arc::clone(
            &self
                .permit
                .as_ref()
                .expect("finalizer reservation owns its permit")
                .state,
        );
        let job = Arc::new(DroppedProcessJobState {
            inner: Mutex::new(Some(DroppedProcessJobInner {
                domain,
                _permit: self
                    .permit
                    .take()
                    .expect("finalizer reservation owns its permit until handoff"),
            })),
            completed: AtomicBool::new(false),
        });
        let sender = self
            .sender
            .take()
            .expect("finalizer reservation owns its sender until handoff");
        match sender.send(job) {
            Ok(()) => {}
            Err(SendError(job)) => {
                state.healthy.store(false, Ordering::Release);
                state.close_admission();
                drop(job);
            }
        }
    }
}

struct DropFinalizerPermit {
    state: Arc<DropFinalizerState>,
}

impl Drop for DropFinalizerPermit {
    fn drop(&mut self) {
        self.state.owned.fetch_sub(1, Ordering::AcqRel);
    }
}

type DroppedProcessJob = Arc<DroppedProcessJobState>;

struct DroppedProcessJobState {
    inner: Mutex<Option<DroppedProcessJobInner>>,
    completed: AtomicBool,
}

struct DroppedProcessJobInner {
    domain: DroppedProcessDomain,
    _permit: DropFinalizerPermit,
}

impl Drop for DroppedProcessJobState {
    fn drop(&mut self) {
        if !self.completed.load(Ordering::Acquire) {
            let inner = self
                .inner
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(inner) = inner {
                std::mem::forget(inner);
            }
        }
    }
}

/// Prepared host resources and their pre-reserved cleanup ownership.
pub(super) struct PreparedProcessOwnership {
    prepared: Option<PreparedHostProcessAttempt>,
    reservation: Option<DropFinalizerReservation>,
}

impl PreparedProcessOwnership {
    pub(super) fn new(
        prepared: PreparedHostProcessAttempt,
        reservation: DropFinalizerReservation,
    ) -> Self {
        Self {
            prepared: Some(prepared),
            reservation: Some(reservation),
        }
    }

    pub(super) fn into_parts(mut self) -> (PreparedHostProcessAttempt, DropFinalizerReservation) {
        (
            self.prepared
                .take()
                .expect("prepared process ownership owns host resources"),
            self.reservation
                .take()
                .expect("prepared process ownership owns finalizer admission"),
        )
    }
}

impl Drop for PreparedProcessOwnership {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let host = prepared.into_cleanup_domain();
        self.reservation
            .take()
            .expect("prepared process ownership owns finalizer admission")
            .submit(unspawned_process_domain(host));
    }
}

/// Attached host resources retained across process spawn failure.
pub(super) struct AttachedProcessOwnership {
    host: Option<AttemptProcessDomain>,
    reservation: Option<DropFinalizerReservation>,
}

impl AttachedProcessOwnership {
    pub(super) fn new(host: AttemptProcessDomain, reservation: DropFinalizerReservation) -> Self {
        Self {
            host: Some(host),
            reservation: Some(reservation),
        }
    }

    pub(super) fn into_parts(mut self) -> (AttemptProcessDomain, DropFinalizerReservation) {
        (
            self.host
                .take()
                .expect("attached process ownership owns its host domain"),
            self.reservation
                .take()
                .expect("attached process ownership owns finalizer admission"),
        )
    }
}

impl Drop for AttachedProcessOwnership {
    fn drop(&mut self) {
        let Some(host) = self.host.take() else {
            return;
        };
        self.reservation
            .take()
            .expect("attached process ownership owns finalizer admission")
            .submit(unspawned_process_domain(host));
    }
}

fn unspawned_process_domain(host: AttemptProcessDomain) -> DroppedProcessDomain {
    #[cfg(unix)]
    let group = ProcessGroupState::Released;
    #[cfg(not(unix))]
    let group = ();
    DroppedProcessDomain::new(None, Some(host), group, LeaderState::Reaped)
}

struct DroppedProcessDomain {
    child: Option<ProcessChild>,
    host: Option<AttemptProcessDomain>,
    group: DroppedProcessGroup,
    leader: LeaderState,
    tree_handled: bool,
    os_error_backoffs: DroppedProcessBackoffs,
    quarantined: bool,
    #[cfg(test)]
    poll_gate: Option<Arc<AtomicBool>>,
}

#[repr(usize)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum DroppedProcessOperation {
    TerminateTree,
    TerminateGroup,
    TerminateLeader,
    TryWait,
    Cleanup,
}

impl DroppedProcessOperation {
    const COUNT: usize = 5;
}

#[derive(Clone, Copy)]
struct DroppedProcessBackoff {
    attempt: usize,
    retry_after: Instant,
}

struct DroppedProcessBackoffs {
    operations: [DroppedProcessBackoff; DroppedProcessOperation::COUNT],
}

impl DroppedProcessBackoffs {
    fn new(now: Instant) -> Self {
        Self {
            operations: [DroppedProcessBackoff {
                attempt: 0,
                retry_after: now,
            }; DroppedProcessOperation::COUNT],
        }
    }

    fn operation(&self, operation: DroppedProcessOperation) -> &DroppedProcessBackoff {
        &self.operations[operation as usize]
    }

    fn operation_mut(&mut self, operation: DroppedProcessOperation) -> &mut DroppedProcessBackoff {
        &mut self.operations[operation as usize]
    }

    fn is_ready(&self, operation: DroppedProcessOperation, now: Instant) -> bool {
        now >= self.operation(operation).retry_after
    }

    fn record_error(&mut self, operation: DroppedProcessOperation, now: Instant) -> bool {
        let backoff = self.operation_mut(operation);
        backoff.attempt = backoff.attempt.saturating_add(1);
        backoff.retry_after = now + finalizer_os_error_retry_delay(backoff.attempt - 1);
        backoff.attempt >= CLEANUP_ATTEMPTS
    }

    fn clear(&mut self, operation: DroppedProcessOperation) {
        self.operation_mut(operation).attempt = 0;
    }
}

impl DroppedProcessDomain {
    fn new(
        child: Option<ProcessChild>,
        host: Option<AttemptProcessDomain>,
        group: DroppedProcessGroup,
        leader: LeaderState,
    ) -> Self {
        let now = Instant::now();
        Self {
            child,
            host,
            group,
            leader,
            tree_handled: false,
            os_error_backoffs: DroppedProcessBackoffs::new(now),
            quarantined: false,
            #[cfg(test)]
            poll_gate: None,
        }
    }

    /// Advances termination, reap, and cleanup without tracing or a Tokio runtime.
    ///
    /// This path does not emit diagnostics or format errors.
    /// It retains the child until `try_wait` reaps it or reports lost ownership.
    fn poll(&mut self, now: Instant) -> bool {
        #[cfg(test)]
        if self
            .poll_gate
            .as_ref()
            .is_some_and(|gate| !gate.load(Ordering::Acquire))
        {
            return false;
        }
        if self.quarantined {
            return false;
        }
        if !self.advance_termination(now) {
            return false;
        }
        if !self.advance_reap(now) {
            return false;
        }
        self.advance_cleanup(now)
    }

    fn advance_termination(&mut self, now: Instant) -> bool {
        let mut termination_ready = true;
        if !self.tree_handled {
            if let Some(host) = self.host.as_mut() {
                if self
                    .os_error_backoffs
                    .is_ready(DroppedProcessOperation::TerminateTree, now)
                {
                    match host.terminate_tree() {
                        Ok(_) => {
                            self.tree_handled = true;
                            self.os_error_backoffs
                                .clear(DroppedProcessOperation::TerminateTree);
                        }
                        Err(_) => {
                            self.quarantined |= self
                                .os_error_backoffs
                                .record_error(DroppedProcessOperation::TerminateTree, now);
                            termination_ready = false;
                        }
                    }
                } else {
                    termination_ready = false;
                }
            } else {
                self.tree_handled = true;
                self.os_error_backoffs
                    .clear(DroppedProcessOperation::TerminateTree);
            }
        }

        #[cfg(unix)]
        {
            if self
                .os_error_backoffs
                .is_ready(DroppedProcessOperation::TerminateGroup, now)
            {
                match terminate_process_group(&mut self.group, self.leader) {
                    Ok(()) => self
                        .os_error_backoffs
                        .clear(DroppedProcessOperation::TerminateGroup),
                    Err(_) => {
                        self.quarantined |= self
                            .os_error_backoffs
                            .record_error(DroppedProcessOperation::TerminateGroup, now);
                        termination_ready = false;
                    }
                }
            } else {
                termination_ready = false;
            }
        }
        #[cfg(not(unix))]
        self.os_error_backoffs
            .clear(DroppedProcessOperation::TerminateGroup);

        if self
            .os_error_backoffs
            .is_ready(DroppedProcessOperation::TerminateLeader, now)
        {
            match terminate_dropped_leader(&mut self.child, &mut self.leader) {
                Ok(()) => self
                    .os_error_backoffs
                    .clear(DroppedProcessOperation::TerminateLeader),
                Err(_) => {
                    self.quarantined |= self
                        .os_error_backoffs
                        .record_error(DroppedProcessOperation::TerminateLeader, now);
                    termination_ready = false;
                }
            }
        } else {
            termination_ready = false;
        }
        termination_ready
    }

    fn advance_reap(&mut self, now: Instant) -> bool {
        let Some(child) = self.child.as_mut() else {
            self.os_error_backoffs
                .clear(DroppedProcessOperation::TryWait);
            return true;
        };
        if !self
            .os_error_backoffs
            .is_ready(DroppedProcessOperation::TryWait, now)
        {
            return false;
        }

        match child.try_wait() {
            Ok(Some(_)) => {
                self.os_error_backoffs
                    .clear(DroppedProcessOperation::TryWait);
                self.leader = LeaderState::Reaped;
                self.child.take();
                true
            }
            Ok(None) => {
                self.os_error_backoffs
                    .clear(DroppedProcessOperation::TryWait);
                false
            }
            Err(error) if wait_ownership_was_lost(&error) => {
                self.os_error_backoffs
                    .clear(DroppedProcessOperation::TryWait);
                self.leader = LeaderState::WaitOwnershipLost;
                self.child.take();
                true
            }
            Err(_) => {
                self.quarantined |= self
                    .os_error_backoffs
                    .record_error(DroppedProcessOperation::TryWait, now);
                false
            }
        }
    }

    fn advance_cleanup(&mut self, now: Instant) -> bool {
        let Some(host) = self.host.as_mut() else {
            self.os_error_backoffs
                .clear(DroppedProcessOperation::Cleanup);
            return true;
        };
        if !self
            .os_error_backoffs
            .is_ready(DroppedProcessOperation::Cleanup, now)
        {
            return false;
        }

        match host.cleanup() {
            Ok(()) => {
                self.os_error_backoffs
                    .clear(DroppedProcessOperation::Cleanup);
                self.host.take();
                true
            }
            Err(_) => {
                self.quarantined |= self
                    .os_error_backoffs
                    .record_error(DroppedProcessOperation::Cleanup, now);
                false
            }
        }
    }
}

fn terminate_dropped_leader(
    child: &mut Option<ProcessChild>,
    leader: &mut LeaderState,
) -> io::Result<()> {
    match *leader {
        LeaderState::ExitedObserved
        | LeaderState::KillRequested
        | LeaderState::Reaped
        | LeaderState::WaitOwnershipLost => Ok(()),
        LeaderState::Running => {
            let child = child.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "subprocess leader is missing")
            })?;
            child.start_kill()?;
            *leader = LeaderState::KillRequested;
            Ok(())
        }
    }
}

fn run_drop_finalizer(state: Arc<DropFinalizerState>, receiver: &Receiver<DroppedProcessJob>) {
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_drop_finalizer_loop(&state, receiver);
        }));
        match result {
            Ok(()) => break,
            Err(payload) => {
                state.healthy.store(false, Ordering::Release);
                state.close_admission();
                std::mem::forget(payload);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    state.worker_stopped.store(true, Ordering::Release);
}

const DROP_FINALIZER_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn run_drop_finalizer_loop(
    state: &Arc<DropFinalizerState>,
    receiver: &Receiver<DroppedProcessJob>,
) {
    let mut input_closed = false;

    loop {
        let active_empty = lock_active(state).is_empty();
        if !input_closed {
            let incoming = if active_empty {
                receiver.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                receiver.recv_timeout(DROP_FINALIZER_POLL_INTERVAL)
            };
            match incoming {
                Ok(job) => {
                    let mut active = lock_active(state);
                    active.push(job);
                    drop(active);
                    #[cfg(test)]
                    if state.panic_worker_once.swap(false, Ordering::AcqRel) {
                        panic!("injected subprocess finalizer worker panic");
                    }
                    let mut active = lock_active(state);
                    active.extend(receiver.try_iter());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => input_closed = true,
            }
        }
        let jobs = lock_active(state).clone();
        if jobs.is_empty() {
            if input_closed {
                return;
            }
            continue;
        }

        let now = Instant::now();
        let mut completed_jobs = Vec::new();
        let mut quarantined_jobs = Vec::new();
        for job in jobs {
            if job.completed.load(Ordering::Acquire) {
                completed_jobs.push(Arc::clone(&job));
                continue;
            }
            let mut inner = lock_job(&job);
            let Some(inner) = inner.as_mut() else {
                job.completed.store(true, Ordering::Release);
                completed_jobs.push(Arc::clone(&job));
                continue;
            };
            let completed =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.domain.poll(now)));
            match completed {
                Ok(true) => {
                    job.completed.store(true, Ordering::Release);
                    completed_jobs.push(Arc::clone(&job));
                }
                Ok(false) if inner.domain.quarantined => {
                    quarantined_jobs.push(Arc::clone(&job));
                }
                Ok(false) => {}
                Err(payload) => {
                    std::mem::forget(payload);
                    quarantined_jobs.push(Arc::clone(&job));
                }
            }
        }

        if !completed_jobs.is_empty() || !quarantined_jobs.is_empty() {
            lock_active(state).retain(|job| {
                !completed_jobs
                    .iter()
                    .any(|completed| Arc::ptr_eq(job, completed))
                    && !quarantined_jobs
                        .iter()
                        .any(|quarantined| Arc::ptr_eq(job, quarantined))
            });
        }
        if !quarantined_jobs.is_empty() {
            state.healthy.store(false, Ordering::Release);
            state.close_admission();
            state
                .quarantined
                .fetch_add(quarantined_jobs.len(), Ordering::AcqRel);
            lock_quarantine(state).extend(quarantined_jobs);
        }
        let active_remains = input_closed && !lock_active(state).is_empty();
        if active_remains {
            std::thread::sleep(DROP_FINALIZER_POLL_INTERVAL);
            #[cfg(test)]
            state
                .input_closed_active_sleeps
                .fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn lock_admission(state: &DropFinalizerState) -> MutexGuard<'_, DropFinalizerAdmission> {
    match state.admission.lock() {
        Ok(admission) => admission,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_active(state: &DropFinalizerState) -> MutexGuard<'_, Vec<DroppedProcessJob>> {
    match state.active.lock() {
        Ok(active) => active,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_quarantine(state: &DropFinalizerState) -> MutexGuard<'_, Vec<DroppedProcessJob>> {
    match state.quarantine.lock() {
        Ok(quarantine) => quarantine,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_job(job: &DroppedProcessJob) -> MutexGuard<'_, Option<DroppedProcessJobInner>> {
    match job.inner.lock() {
        Ok(inner) => inner,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn wait_ownership_was_lost(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ECHILD)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

const CLEANUP_ATTEMPTS: usize = 10;

async fn cleanup_host_domain(host: &mut AttemptProcessDomain) -> io::Result<()> {
    for attempt in 0..CLEANUP_ATTEMPTS {
        match host.cleanup() {
            Ok(()) => return Ok(()),
            Err(error) if attempt + 1 == CLEANUP_ATTEMPTS => return Err(error),
            Err(_) => {
                tokio::time::sleep(cleanup_retry_delay(attempt)).await;
            }
        }
    }
    unreachable!("cleanup loop always returns")
}

fn cleanup_retry_delay(attempt: usize) -> Duration {
    const MAX_DELAY: Duration = Duration::from_secs(1);

    let multiplier = u32::try_from(attempt.saturating_add(1)).unwrap_or(u32::MAX);
    Duration::from_millis(10)
        .saturating_mul(multiplier)
        .min(MAX_DELAY)
}

fn finalizer_os_error_retry_delay(attempt: usize) -> Duration {
    cleanup_retry_delay(attempt)
}

/// Preserves every failed termination boundary.
fn finish_termination(
    tree: io::Result<DomainTermination>,
    group: io::Result<()>,
    leader: io::Result<()>,
) -> io::Result<()> {
    let process = combine_errors("process group", group, "leader", leader);
    combine_errors("cgroup", tree.map(|_| ()), "process", process)
}

fn combine_errors(
    left_name: &str,
    left: io::Result<()>,
    right_name: &str,
    right: io::Result<()>,
) -> io::Result<()> {
    match (left, right) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(left), Err(right)) => Err(io::Error::new(
            left.kind(),
            format!(
                "{left_name} termination failed: {left}; {right_name} termination failed: {right}"
            ),
        )),
    }
}

/// Checks for an exited child without consuming its wait status.
#[cfg(unix)]
fn exited_without_reaping(pid: libc::pid_t) -> io::Result<bool> {
    loop {
        // SAFETY: zero is a valid initial representation for `siginfo_t` passed to `waitid`.
        // The kernel initializes it before a reported state change.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `pid` names a child and `info` points to writable storage.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(info.si_signo != 0);
        }

        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests;
