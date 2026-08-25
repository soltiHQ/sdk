use super::*;

fn empty_dropped_domain() -> DroppedProcessDomain {
    #[cfg(unix)]
    let group = ProcessGroupState::Released;
    #[cfg(not(unix))]
    let group = ();

    DroppedProcessDomain::new(None, None, group, LeaderState::Reaped)
}

async fn wait_for_status(
    finalizer: &DropFinalizerDomain,
    predicate: impl Fn(SubprocessFinalizerStatus) -> bool,
) -> SubprocessFinalizerStatus {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = finalizer.status();
            if predicate(status) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("finalizer state did not converge")
}

#[tokio::test]
async fn finalizer_admission_is_bounded_and_shutdown_is_recoverable() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    let reservation = finalizer.try_reserve().unwrap();
    let full = match finalizer.try_reserve() {
        Ok(_) => panic!("second reservation must exceed capacity"),
        Err(error) => error,
    };
    assert_eq!(full.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(finalizer.status().owned(), 1);

    let timed_out = finalizer
        .shutdown(Duration::from_millis(1))
        .await
        .unwrap_err();
    assert_eq!(timed_out.kind(), io::ErrorKind::TimedOut);
    assert!(!finalizer.status().accepting());

    drop(reservation);
    finalizer.shutdown(Duration::from_secs(2)).await.unwrap();
    let status = finalizer.status();
    assert_eq!(status.owned(), 0);
    assert!(status.healthy());
    assert!(!status.accepting());
}

#[tokio::test]
async fn maximum_supported_capacity_is_constructed_lazily() {
    let capacity = u32::MAX as usize;
    let finalizer = DropFinalizerDomain::start(capacity).unwrap();
    assert_eq!(finalizer.status().capacity(), capacity);

    drop(finalizer.try_reserve().unwrap());
    assert_eq!(finalizer.status().owned(), 0);
    finalizer.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn shutdown_can_be_canceled_and_retried() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    let reservation = finalizer.try_reserve().unwrap();
    let shutdown_finalizer = finalizer.clone();
    let shutdown =
        tokio::spawn(async move { shutdown_finalizer.shutdown(Duration::from_secs(30)).await });

    wait_for_status(&finalizer, |status| !status.accepting()).await;
    shutdown.abort();
    assert!(shutdown.await.unwrap_err().is_cancelled());
    assert!(!finalizer.status().accepting());

    drop(reservation);
    finalizer.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(finalizer.status().owned(), 0);
}

#[tokio::test]
async fn shutdown_wakes_when_the_final_reservation_releases_and_worker_stops() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    let reservation = finalizer.try_reserve().unwrap();
    let shutdown_finalizer = finalizer.clone();
    let started = std::time::Instant::now();
    let shutdown =
        tokio::spawn(async move { shutdown_finalizer.shutdown(Duration::from_secs(2)).await });

    wait_for_status(&finalizer, |status| !status.accepting()).await;
    drop(reservation);

    shutdown.await.unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "terminal worker notification waited until the shutdown deadline"
    );
    assert_eq!(finalizer.status().owned(), 0);
}

#[tokio::test]
async fn repeated_shutdown_of_drained_finalizer_succeeds_without_waiting() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();

    let started = std::time::Instant::now();
    finalizer.shutdown(Duration::from_secs(2)).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "drained finalizer waited until the shutdown deadline"
    );
    finalizer.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn overflowing_shutdown_timeout_is_rejected_without_closing_admission() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();

    let error = finalizer.shutdown(Duration::MAX).await.unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(finalizer.status().accepting());
    drop(finalizer.try_reserve().unwrap());
    finalizer.shutdown(Duration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn disconnected_input_with_active_work_sleeps_between_polls() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    let gate = Arc::new(AtomicBool::new(false));
    let mut domain = empty_dropped_domain();
    domain.poll_gate = Some(Arc::clone(&gate));
    finalizer.try_reserve().unwrap().submit(domain);
    let started = Instant::now();
    finalizer.inner.state.close_admission();

    tokio::time::timeout(Duration::from_secs(2), async {
        while finalizer
            .inner
            .state
            .input_closed_active_sleeps
            .load(Ordering::Acquire)
            < 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("finalizer did not pace active work after input closed");

    assert!(
        started.elapsed() >= DROP_FINALIZER_POLL_INTERVAL.saturating_mul(2),
        "closed-input polling completed without the required delay"
    );
    assert_eq!(finalizer.status().owned(), 1);
    gate.store(true, Ordering::Release);
    finalizer.shutdown(Duration::from_secs(2)).await.unwrap();
    assert_eq!(finalizer.status().owned(), 0);
}

#[tokio::test]
async fn shutdown_reloads_health_after_join_and_repeats_the_terminal_error() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    finalizer
        .inner
        .state
        .unhealthy_before_join_once
        .store(true, Ordering::Release);

    let first = finalizer
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(first.kind(), io::ErrorKind::BrokenPipe);
    assert!(!finalizer.status().healthy());

    let repeated = finalizer
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(repeated.kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn persistent_cleanup_error_is_quarantined_and_charged() {
    let parent = tempfile::TempDir::new().unwrap();
    let invalid_cgroup = parent.path().join("not-a-directory");
    std::fs::write(&invalid_cgroup, b"owned").unwrap();
    let host = AttemptProcessDomain::for_test(invalid_cgroup);
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    finalizer
        .try_reserve()
        .unwrap()
        .submit(unspawned_process_domain(host));

    let status = wait_for_status(&finalizer, |status| status.quarantined() == 1).await;
    assert_eq!(status.owned(), 1);
    assert!(!status.healthy());
    assert!(!status.accepting());
    let unavailable = match finalizer.try_reserve() {
        Ok(_) => panic!("quarantined finalizer must reject admission"),
        Err(error) => error,
    };
    assert_eq!(unavailable.kind(), io::ErrorKind::BrokenPipe);
    let shutdown = finalizer
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(shutdown.kind(), io::ErrorKind::Other);
}

#[tokio::test]
async fn worker_panic_keeps_active_and_queued_ownership_fail_closed() {
    let finalizer = DropFinalizerDomain::start(2).unwrap();
    let first = finalizer.try_reserve().unwrap();
    let second = finalizer.try_reserve().unwrap();
    finalizer
        .inner
        .state
        .panic_worker_once
        .store(true, Ordering::Release);

    first.submit(empty_dropped_domain());
    second.submit(empty_dropped_domain());

    let status = wait_for_status(&finalizer, |status| {
        !status.healthy() && status.owned() == 0
    })
    .await;
    assert!(!status.accepting());
    assert_eq!(status.quarantined(), 0);
    let shutdown = finalizer
        .shutdown(Duration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(shutdown.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn finalizer_os_errors_back_off_independently_per_operation() {
    let now = Instant::now();
    let mut delayed = DroppedProcessBackoffs::new(now);
    let unaffected = DroppedProcessBackoffs::new(now);

    let _ = delayed.record_error(DroppedProcessOperation::TerminateTree, now);
    let _ = delayed.record_error(DroppedProcessOperation::TerminateGroup, now);
    let first_retry = now + finalizer_os_error_retry_delay(0);
    assert!(!delayed.is_ready(DroppedProcessOperation::TerminateTree, now));
    assert!(!delayed.is_ready(DroppedProcessOperation::TerminateGroup, now));
    assert!(delayed.is_ready(DroppedProcessOperation::TerminateTree, first_retry));
    assert!(delayed.is_ready(DroppedProcessOperation::TerminateGroup, first_retry));
    assert!(unaffected.is_ready(DroppedProcessOperation::TerminateTree, now));
    assert!(unaffected.is_ready(DroppedProcessOperation::TerminateGroup, now));

    let _ = delayed.record_error(DroppedProcessOperation::TerminateTree, first_retry);
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
    let _ = delayed.record_error(DroppedProcessOperation::TerminateTree, second_retry);
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
    let mut delayed = DroppedProcessDomain::new(None, None, delayed_group, LeaderState::Running);
    let mut ready = empty_dropped_domain();
    let now = Instant::now();

    let _ = delayed
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
fn active_domain(child: tokio::process::Child, host: AttemptProcessDomain) -> ActiveProcessDomain {
    let finalizer = DropFinalizerDomain::start(32).unwrap();
    ActiveProcessDomain::new(
        child,
        host,
        Arc::from("test"),
        finalizer.try_reserve().unwrap(),
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
