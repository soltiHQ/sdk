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
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use tracing::warn;

use crate::host::{AttemptProcessDomain, DomainTermination};
use crate::subprocess::child::{ChildOutput, ProcessChild};

/// Child process and termination boundary for one active attempt.
///
/// Drop requests termination before scheduling reap and cleanup.
pub(super) struct ActiveProcessDomain {
    child: Option<ProcessChild>,
    host: Option<AttemptProcessDomain>,
    drop_finalizer: DropFinalizerHandle,
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
        drop_finalizer: DropFinalizerHandle,
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
            drop_finalizer,
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
            .submit(DroppedProcessDomain::new(child, host, group, self.leader));
    }
}

/// Handle prepared before a subprocess may be spawned.
#[derive(Clone)]
pub(super) struct DropFinalizerHandle {
    state: Arc<DropFinalizerState>,
}

impl DropFinalizerHandle {
    fn submit(&self, domain: DroppedProcessDomain) {
        let mut inbox = lock_inbox(&self.state);
        inbox.push(Arc::new(DroppedProcessJobState {
            domain: Mutex::new(domain),
            mode: AtomicU8::new(JOB_NORMAL),
            completed: AtomicBool::new(false),
        }));
        drop(inbox);
        self.state.wake.notify_one();
    }
}

const JOB_NORMAL: u8 = 0;
const JOB_RECOVERY: u8 = 1;
const JOB_TERMINAL: u8 = 2;

type DroppedProcessJob = Arc<DroppedProcessJobState>;

struct DroppedProcessJobState {
    domain: Mutex<DroppedProcessDomain>,
    mode: AtomicU8,
    completed: AtomicBool,
}

struct DropFinalizerState {
    inbox: Mutex<Vec<DroppedProcessJob>>,
    active: Mutex<Vec<DroppedProcessJob>>,
    wake: Condvar,
    healthy: AtomicBool,
}

struct DropFinalizer {
    state: Arc<DropFinalizerState>,
    thread: std::thread::JoinHandle<()>,
}

static DROP_FINALIZER: OnceLock<Mutex<Option<DropFinalizer>>> = OnceLock::new();

/// Starts the Tokio-independent drop finalizer before process creation.
pub(super) fn prepare_drop_finalizer() -> io::Result<DropFinalizerHandle> {
    let finalizer = DROP_FINALIZER.get_or_init(|| Mutex::new(None));
    let mut finalizer = finalizer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(finalizer) = finalizer.as_ref() {
        if finalizer.thread.is_finished() || !finalizer.state.healthy.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "subprocess drop finalizer stopped",
            ));
        }
        return Ok(DropFinalizerHandle {
            state: Arc::clone(&finalizer.state),
        });
    }

    let started = start_drop_finalizer()?;
    let state = Arc::clone(&started.state);
    *finalizer = Some(started);
    Ok(DropFinalizerHandle { state })
}

fn start_drop_finalizer() -> io::Result<DropFinalizer> {
    let state = Arc::new(DropFinalizerState {
        inbox: Mutex::new(Vec::new()),
        active: Mutex::new(Vec::new()),
        wake: Condvar::new(),
        healthy: AtomicBool::new(true),
    });
    let worker_state = Arc::clone(&state);
    let thread = std::thread::Builder::new()
        .name("solti-exec-reaper".into())
        .spawn(move || run_drop_finalizer(worker_state))
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to start subprocess drop finalizer: {error}"),
            )
        })?;
    Ok(DropFinalizer { state, thread })
}

struct DroppedProcessDomain {
    child: Option<ProcessChild>,
    host: Option<AttemptProcessDomain>,
    group: DroppedProcessGroup,
    leader: LeaderState,
    tree_handled: bool,
    os_error_backoffs: DroppedProcessBackoffs,
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

    fn record_error(&mut self, operation: DroppedProcessOperation, now: Instant) {
        let backoff = self.operation_mut(operation);
        backoff.attempt = backoff.attempt.saturating_add(1);
        backoff.retry_after = now + finalizer_os_error_retry_delay(backoff.attempt - 1);
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
        }
    }

    /// Advances termination, reap, and cleanup without tracing or a Tokio runtime.
    ///
    /// This path does not emit diagnostics or format errors.
    /// It retains the child until `try_wait` reaps it or reports lost ownership.
    fn poll(&mut self, now: Instant) -> bool {
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
                            self.os_error_backoffs
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
                        self.os_error_backoffs
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
                    self.os_error_backoffs
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
                self.os_error_backoffs
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
                self.os_error_backoffs
                    .record_error(DroppedProcessOperation::Cleanup, now);
                false
            }
        }
    }

    /// Completes ownership after an unexpected panic in the regular poll path.
    ///
    /// This terminal path contains no tracing, formatting, callbacks, or unwraps.
    /// It does not release the child, group, or cgroup before completion.
    fn finalize_after_panic(&mut self) {
        const POLL_INTERVAL: Duration = Duration::from_millis(10);

        loop {
            if self.poll(Instant::now()) {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
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

fn run_drop_finalizer(state: Arc<DropFinalizerState>) {
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_drop_finalizer_loop(&state);
        }));
        if result.is_err() {
            state.healthy.store(false, Ordering::Release);
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn run_drop_finalizer_loop(state: &Arc<DropFinalizerState>) {
    const POLL_INTERVAL: Duration = Duration::from_millis(10);

    loop {
        let active_empty = lock_active(state).is_empty();
        let mut inbox = lock_inbox(state);
        if inbox.is_empty() {
            inbox = if active_empty {
                match state.wake.wait(inbox) {
                    Ok(inbox) => inbox,
                    Err(poisoned) => poisoned.into_inner(),
                }
            } else {
                match state.wake.wait_timeout(inbox, POLL_INTERVAL) {
                    Ok((inbox, _)) => inbox,
                    Err(poisoned) => poisoned.into_inner().0,
                }
            };
        }
        let incoming = std::mem::take(&mut *inbox);
        drop(inbox);

        if !incoming.is_empty() {
            lock_active(state).extend(incoming);
        }
        let jobs = lock_active(state).clone();
        if jobs.is_empty() {
            continue;
        }

        let now = Instant::now();
        let mut completed_jobs = Vec::new();
        for job in jobs {
            if job.completed.load(Ordering::Acquire) {
                completed_jobs.push(Arc::clone(&job));
                continue;
            }
            let mode = job.mode.load(Ordering::Acquire);
            if mode == JOB_TERMINAL {
                continue;
            }
            let mut domain = lock_domain(&job);
            let completed =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| domain.poll(now)));
            match completed {
                Ok(true) => completed_jobs.push(Arc::clone(&job)),
                Ok(false) => {}
                Err(_) if mode == JOB_NORMAL => {
                    job.mode.store(JOB_RECOVERY, Ordering::Release);
                }
                Err(_) => {
                    drop(domain);
                    start_terminal_recovery(state, &job);
                }
            }
        }

        if !completed_jobs.is_empty() {
            lock_active(state).retain(|job| {
                !completed_jobs
                    .iter()
                    .any(|completed| Arc::ptr_eq(job, completed))
            });
        }
    }
}

fn start_terminal_recovery(state: &Arc<DropFinalizerState>, job: &DroppedProcessJob) {
    if job
        .mode
        .compare_exchange(
            JOB_RECOVERY,
            JOB_TERMINAL,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    state.healthy.store(false, Ordering::Release);

    let terminal_job = Arc::clone(job);
    if std::thread::Builder::new()
        .name("solti-exec-terminal-reaper".into())
        .spawn(move || {
            let mut domain = lock_domain(&terminal_job);
            domain.finalize_after_panic();
            drop(domain);
            terminal_job.completed.store(true, Ordering::Release);
        })
        .is_err()
    {
        let mut domain = lock_domain(job);
        domain.finalize_after_panic();
        drop(domain);
        job.completed.store(true, Ordering::Release);
    }
}

fn lock_inbox(state: &DropFinalizerState) -> MutexGuard<'_, Vec<DroppedProcessJob>> {
    match state.inbox.lock() {
        Ok(inbox) => inbox,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_active(state: &DropFinalizerState) -> MutexGuard<'_, Vec<DroppedProcessJob>> {
    match state.active.lock() {
        Ok(active) => active,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_domain(job: &DroppedProcessJob) -> MutexGuard<'_, DroppedProcessDomain> {
    match job.domain.lock() {
        Ok(domain) => domain,
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
mod tests {
    use super::*;

    fn empty_dropped_domain() -> DroppedProcessDomain {
        #[cfg(unix)]
        let group = ProcessGroupState::Released;
        #[cfg(not(unix))]
        let group = ();

        DroppedProcessDomain::new(None, None, group, LeaderState::Reaped)
    }

    #[test]
    fn finalizer_os_errors_back_off_independently_per_operation() {
        let now = Instant::now();
        let mut delayed = DroppedProcessBackoffs::new(now);
        let unaffected = DroppedProcessBackoffs::new(now);

        delayed.record_error(DroppedProcessOperation::TerminateTree, now);
        delayed.record_error(DroppedProcessOperation::TerminateGroup, now);
        let first_retry = now + finalizer_os_error_retry_delay(0);
        assert!(!delayed.is_ready(DroppedProcessOperation::TerminateTree, now));
        assert!(!delayed.is_ready(DroppedProcessOperation::TerminateGroup, now));
        assert!(delayed.is_ready(DroppedProcessOperation::TerminateTree, first_retry));
        assert!(delayed.is_ready(DroppedProcessOperation::TerminateGroup, first_retry));
        assert!(unaffected.is_ready(DroppedProcessOperation::TerminateTree, now));
        assert!(unaffected.is_ready(DroppedProcessOperation::TerminateGroup, now));

        delayed.record_error(DroppedProcessOperation::TerminateTree, first_retry);
        let second_retry = first_retry + finalizer_os_error_retry_delay(1);
        assert!(!delayed.is_ready(DroppedProcessOperation::TerminateTree, first_retry));
        assert!(delayed.is_ready(DroppedProcessOperation::TerminateTree, second_retry));
        assert!(delayed.is_ready(DroppedProcessOperation::TerminateGroup, first_retry));
        assert_eq!(
            delayed
                .operation(DroppedProcessOperation::TerminateTree)
                .attempt,
            2
        );
        assert_eq!(
            delayed
                .operation(DroppedProcessOperation::TerminateGroup)
                .attempt,
            1
        );

        delayed.clear(DroppedProcessOperation::TerminateTree);
        delayed.record_error(DroppedProcessOperation::TerminateTree, second_retry);
        assert_eq!(
            delayed
                .operation(DroppedProcessOperation::TerminateTree)
                .retry_after,
            second_retry + finalizer_os_error_retry_delay(0)
        );
    }

    #[test]
    fn finalizer_backoff_does_not_delay_an_unaffected_successful_job() {
        #[cfg(unix)]
        let delayed_group = ProcessGroupState::Released;
        #[cfg(not(unix))]
        let delayed_group = ();
        let mut delayed =
            DroppedProcessDomain::new(None, None, delayed_group, LeaderState::Running);
        let mut ready = empty_dropped_domain();
        let now = Instant::now();

        delayed
            .os_error_backoffs
            .record_error(DroppedProcessOperation::TerminateLeader, now);

        assert!(!delayed.poll(now));
        assert!(ready.poll(now));
        let retry = now + finalizer_os_error_retry_delay(0);
        assert!(!delayed.poll(retry));
        assert_eq!(
            delayed
                .os_error_backoffs
                .operation(DroppedProcessOperation::TerminateLeader)
                .attempt,
            2
        );
    }

    #[test]
    fn successful_tree_termination_still_requires_process_group_defense() {
        let error = io::Error::other("process group failed");
        let result = finish_termination(Ok(DomainTermination::Requested), Err(error), Ok(()));
        assert_eq!(result.unwrap_err().to_string(), "process group failed");
    }

    #[test]
    fn tree_and_process_group_termination_can_succeed_together() {
        assert!(finish_termination(Ok(DomainTermination::Requested), Ok(()), Ok(())).is_ok());
    }

    #[test]
    fn unavailable_tree_termination_uses_fallback() {
        let error = io::Error::other("fallback failed");
        let result = finish_termination(Ok(DomainTermination::Unavailable), Err(error), Ok(()));
        assert_eq!(result.unwrap_err().to_string(), "fallback failed");
    }

    #[test]
    fn tree_error_is_preserved_after_successful_fallback() {
        let result = finish_termination(Err(io::Error::other("tree failed")), Ok(()), Ok(()));
        assert_eq!(result.unwrap_err().to_string(), "tree failed");
    }

    #[test]
    fn group_and_leader_errors_are_both_preserved() {
        let result = finish_termination(
            Ok(DomainTermination::Unavailable),
            Err(io::Error::other("group failed")),
            Err(io::Error::other("leader failed")),
        );

        assert_eq!(
            result.unwrap_err().to_string(),
            "process group termination failed: group failed; leader termination failed: leader failed"
        );
    }

    #[cfg(unix)]
    fn empty_host_domain(command: &mut tokio::process::Command) -> AttemptProcessDomain {
        crate::host::HostProcessPolicy::new()
            .prepare()
            .unwrap()
            .prepare_attempt(None)
            .unwrap()
            .apply_to_command(command.as_std_mut())
    }

    #[cfg(unix)]
    fn active_domain(
        child: tokio::process::Child,
        host: AttemptProcessDomain,
    ) -> ActiveProcessDomain {
        ActiveProcessDomain::new(
            child,
            host,
            Arc::from("test"),
            prepare_drop_finalizer().unwrap(),
        )
    }

    #[cfg(unix)]
    fn assert_pid_reaped(pid: libc::pid_t) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: `kill(pid, 0)` only probes process existence.
            if unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "subprocess leader {pid} was not reaped"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn waitid_observation_preserves_exit_status_until_reap() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("exit 37").process_group(0);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;

        let mut sigchld =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).unwrap();
        while !exited_without_reaping(pid).unwrap() {
            sigchld.recv().await.expect("SIGCHLD listener closed");
        }
        let status = child.wait().await.unwrap();

        assert_eq!(status.code(), Some(37));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_child_try_wait_keeps_the_fast_poll_path() {
        let mut command = tokio::process::Command::new("sleep");
        command.arg("30").process_group(0);
        let child = command.spawn().unwrap();
        let mut dropped = DroppedProcessDomain::new(
            Some(child.into()),
            None,
            ProcessGroupState::Handled,
            LeaderState::KillRequested,
        );
        let now = Instant::now();

        assert!(!dropped.poll(now));
        let wait_backoff = dropped
            .os_error_backoffs
            .operation(DroppedProcessOperation::TryWait);
        assert_eq!(wait_backoff.attempt, 0);
        assert!(wait_backoff.retry_after <= now);

        let child = dropped.child.as_mut().expect("live child remains owned");
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        dropped.child.take();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_reap_releases_the_numeric_process_group_identity() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("exit 0").process_group(0);
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let mut active = active_domain(child, host);

        let mut status = 0;
        // SAFETY: `pid` names the owned child and `status` is writable.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);

        let error = active.observe_exit().await.unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
        assert_eq!(active.leader, LeaderState::WaitOwnershipLost);
        assert!(active.process_group_id().is_none());
        assert_eq!(
            active.terminate().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        active.terminated = true;
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn observed_exited_leader_accepts_macos_group_eperm() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("exit 0").process_group(0);
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let mut active = active_domain(child, host);

        active.observe_exit().await.unwrap();
        assert!(macos_group_contains_only_leader(active.process_group_id().unwrap()).unwrap());
        active.terminate().unwrap();
        let status = active.reap().await.unwrap();

        assert!(status.success());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_group_inspection_detects_a_live_descendant() {
        let directory = tempfile::TempDir::new().unwrap();
        let marker = directory.path().join("descendant.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30 & printf '%s\\n' \"$!\" > \"$1\"; exit 0")
            .arg("sh")
            .arg(&marker)
            .process_group(0);
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let mut active = active_domain(child, host);

        active.observe_exit().await.unwrap();
        let descendant = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        // SAFETY: `kill(pid, 0)` only probes process existence.
        assert_eq!(unsafe { libc::kill(descendant, 0) }, 0);
        assert!(!macos_group_contains_only_leader(active.process_group_id().unwrap()).unwrap());

        active.terminate().unwrap();
        active.reap().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reaped_domain_cannot_signal_a_numeric_process_group() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("sleep 30").process_group(0);
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let mut active = active_domain(child, host);

        active.terminate().unwrap();
        active.reap().await.unwrap();

        assert!(active.process_group_id().is_none());
        active.terminate().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn armed_process_group_blocks_leader_reap() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("sleep 30").process_group(0);
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let pgid = child.id().unwrap() as libc::pid_t;
        let mut active = active_domain(child, host);

        active.leader = LeaderState::KillRequested;
        assert!(!active.leader_can_be_reaped());
        active.group = ProcessGroupState::Handled;
        assert!(active.leader_can_be_reaped());

        active.leader = LeaderState::Running;
        active.group = ProcessGroupState::Armed(pgid);
        active.terminate().unwrap();
        active.reap().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn escaped_leader_is_terminated_separately_from_its_original_group() {
        // SAFETY: `getpgrp` has no preconditions.
        let controller_pgid = unsafe { libc::getpgrp() };
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg("sleep 30").process_group(0);
        // SAFETY: the hook calls only the async-signal-safe `setpgid` function.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, controller_pgid) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        let mut active = active_domain(child, host);

        // SAFETY: `getpgid` only reads process metadata for a numeric pid.
        assert_ne!(unsafe { libc::getpgid(pid) }, pid);

        active.terminate().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), active.reap())
            .await
            .expect("escaped leader survived termination")
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_synchronously_signals_an_active_group_descendant() {
        let directory = tempfile::TempDir::new().unwrap();
        let marker = directory.path().join("descendant.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30 & child=$!; printf '%s\\n' \"$child\" > \"$1\"; wait \"$child\"")
            .arg("sh")
            .arg(&marker)
            .process_group(0);
        let host = empty_host_domain(&mut command);
        let child = command.spawn().unwrap();
        let leader = child.id().unwrap() as libc::pid_t;
        let active = active_domain(child, host);
        let descendant = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&marker)
                    && let Ok(pid) = value.trim().parse::<libc::pid_t>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("descendant did not report its pid");

        // SAFETY: `getpgid` only reads process metadata for a numeric pid.
        assert_eq!(unsafe { libc::getpgid(descendant) }, leader);

        drop(active);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                // SAFETY: `kill(pid, 0)` only probes process existence.
                if unsafe { libc::kill(descendant, 0) } != 0
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subprocess descendant survived domain drop");
    }

    #[cfg(unix)]
    #[test]
    fn drop_without_runtime_reaps_the_leader() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (active, leader) = runtime.block_on(async {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg("sleep 30").process_group(0);
            let host = empty_host_domain(&mut command);
            let child = command.spawn().unwrap();
            let leader = child.id().unwrap() as libc::pid_t;
            (active_domain(child, host), leader)
        });
        drop(runtime);

        drop(active);

        assert_pid_reaped(leader);
    }

    #[cfg(unix)]
    #[test]
    fn drop_before_runtime_shutdown_reaps_the_leader() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let leader = runtime.block_on(async {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg("sleep 30").process_group(0);
            let host = empty_host_domain(&mut command);
            let child = command.spawn().unwrap();
            let leader = child.id().unwrap() as libc::pid_t;
            drop(active_domain(child, host));
            leader
        });
        drop(runtime);

        assert_pid_reaped(leader);
    }

    #[cfg(unix)]
    #[test]
    fn drop_finalizer_reaps_multiple_leaders() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let leaders = runtime.block_on(async {
            let mut leaders = Vec::new();
            for _ in 0..8 {
                let mut command = tokio::process::Command::new("sh");
                command.arg("-c").arg("sleep 30").process_group(0);
                let host = empty_host_domain(&mut command);
                let child = command.spawn().unwrap();
                leaders.push(child.id().unwrap() as libc::pid_t);
                drop(active_domain(child, host));
            }
            leaders
        });
        drop(runtime);

        for leader in leaders {
            assert_pid_reaped(leader);
        }
    }
}
