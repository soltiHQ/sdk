use super::*;
use crate::output::OutputReaderFailure;
use std::{
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, Waker},
};
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    instrument::WithSubscriber as _,
    span::{Attributes, Id, Record},
};

#[derive(Default)]
struct TraceCapture {
    fields: Mutex<Vec<String>>,
}

struct CaptureSubscriber(Arc<TraceCapture>);

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        event.record(&mut CaptureVisitor(&self.0));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

struct CaptureVisitor<'a>(&'a TraceCapture);

impl Visit for CaptureVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .fields
            .lock()
            .unwrap()
            .push(format!("{}={value:?}", field.name()));
    }
}

#[test]
fn process_lifecycle_errors_are_fatal_and_complete() {
    let mut lifecycle = ProcessLifecycleError::default();
    lifecycle.push(
        "process domain termination",
        std::io::Error::other("termination error"),
    );
    lifecycle.push(
        "leader reap",
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "reap error"),
    );
    lifecycle.push(
        "process domain cleanup",
        std::io::Error::other("cleanup error"),
    );

    let error = TaskError::fatal_from(lifecycle);
    match error {
        TaskError::Fatal { reason, .. } => assert_eq!(
            reason,
            "process domain termination failed: termination error; leader reap failed: reap error; process domain cleanup failed: cleanup error"
        ),
        other => panic!("expected fatal lifecycle error, got {other}"),
    }
}

#[cfg(unix)]
#[test]
fn executable_format_errors_are_permanent_before_context_is_added() {
    let error = task_io_error(
        "spawn failed",
        std::io::Error::from_raw_os_error(libc::ENOEXEC),
    );

    assert!(matches!(error, TaskError::Fatal { .. }));
}

#[test]
fn resolved_debug_redacts_command_arguments_and_script() {
    let resolved = Resolved {
        command: "resolved-command-secret".into(),
        args: vec!["resolved-argument-secret".into()],
        script_body: Some(Arc::from("resolved-script-secret")),
    };

    let formatted = format!("{resolved:?}");
    for secret in [
        "resolved-command-secret",
        "resolved-argument-secret",
        "resolved-script-secret",
    ] {
        assert!(!formatted.contains(secret), "{formatted}");
    }
    assert!(formatted.contains("argument_count"), "{formatted}");
    assert!(formatted.contains("script_present"), "{formatted}");
}

#[tokio::test]
async fn cancellation_wins_a_ready_leader_exit_tie() {
    let completion = observe_attempt_completion(
        &TaskContext::detached_cancelled(),
        std::future::ready(Ok(())),
    )
    .await;

    assert!(matches!(completion, AttemptCompletion::Canceled));
}

#[tokio::test]
async fn cancellation_during_a_mandatory_post_exit_wait_is_latched() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    let cancellation_latched = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let task: TaskRef = TaskFn::arc({
        let entered_tx = Arc::clone(&entered_tx);
        let release_rx = Arc::clone(&release_rx);
        let cancellation_latched = Arc::clone(&cancellation_latched);
        move |cancel: TaskContext| {
            let entered_tx = entered_tx
                .lock()
                .unwrap()
                .take()
                .expect("post-exit test runs one attempt");
            let release_rx = release_rx
                .lock()
                .unwrap()
                .take()
                .expect("post-exit test runs one attempt");
            let cancellation_latched = Arc::clone(&cancellation_latched);
            async move {
                // Model the point immediately after a successful leader-exit
                // observation. The following future represents a mandatory
                // drain, reap, or cleanup operation and may not be dropped.
                let completion = AttemptCompletion::LeaderExited(Ok(()));
                let mut cancellation =
                    AttemptCancellationLatch::after_completion(&cancel, &completion);
                entered_tx.send(()).unwrap();
                let completed = cancellation
                    .complete(async {
                        release_rx.await.expect("mandatory operation release");
                        true
                    })
                    .await;
                assert!(completed, "the mandatory operation must finish");
                cancellation_latched.store(cancellation.is_latched(), Ordering::Release);
                if cancellation.is_latched() {
                    Err(TaskError::Canceled)
                } else {
                    Ok(())
                }
            }
        }
    });

    let supervisor = taskvisor::Supervisor::new(taskvisor::SupervisorConfig::default(), Vec::new());
    let handle = supervisor.serve().unwrap();
    let (id, waiter) = handle
        .add_and_watch(taskvisor::TaskSpec::once("post-exit-cancellation", task))
        .await
        .unwrap();
    entered_rx
        .await
        .expect("attempt must enter its mandatory post-exit wait");

    assert!(handle.remove(id).await.unwrap());
    release_tx
        .send(())
        .expect("mandatory operation must remain owned after cancellation");

    let outcome = waiter.wait().await.unwrap();
    assert_eq!(outcome.kind(), taskvisor::TaskOutcomeKind::Canceled);
    assert!(cancellation_latched.load(Ordering::Acquire));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_physical_post_exit_failure_remains_fatal_after_cancellation() {
    let cancel = TaskContext::detached_cancelled();
    let completion = AttemptCompletion::Canceled;
    let mut cancellation = AttemptCancellationLatch::after_completion(&cancel, &completion);
    let error = cancellation
        .complete(std::future::ready(Err::<(), _>(std::io::Error::other(
            "injected reap failure",
        ))))
        .await
        .unwrap_err();

    assert!(
        cancellation.is_latched(),
        "cancellation must remain latched"
    );
    let mut lifecycle = ProcessLifecycleError::default();
    lifecycle.push("leader reap", error);
    match TaskError::fatal_from(lifecycle) {
        TaskError::Fatal { reason, .. } => {
            assert_eq!(reason, "leader reap failed: injected reap failure");
        }
        other => panic!("physical lifecycle failure must remain fatal, got {other:?}"),
    }
}

#[test]
fn cgroup_name_is_stable() {
    assert_eq!(
        build_cgroup_name("runner", "slot", 42, 1000),
        "runner-slot-2a-3e8"
    );
}

#[test]
fn attempt_counter_rejects_after_identity_limit() {
    let attempts = AtomicU32::new(u32::MAX - 1);

    assert_eq!(next_attempt(&attempts), Some(u32::MAX));
    assert_eq!(next_attempt(&attempts), None);
    assert_eq!(attempts.load(Ordering::Relaxed), u32::MAX);
}

fn assert_future_pending<F: std::future::Future>(future: Pin<&mut F>) {
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(future.poll(&mut context), Poll::Pending));
}

async fn occupy_only_blocking_worker() -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>)
{
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = tokio::task::spawn_blocking(move || {
        let _ = started_tx.send(());
        release_rx.recv().expect("blocking worker release sender");
    });
    started_rx.await.expect("blocking worker did not start");
    (release_tx, blocker)
}

async fn wait_for_finalizer_release(finalizer: &DropFinalizerDomain) {
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while finalizer.status().owned() != 0 {
            tokio::time::sleep(StdDuration::from_millis(1)).await;
        }
    })
    .await
    .expect("finalizer ownership was not released");
}

fn injected_prepare_error() -> crate::host::HostProcessError {
    crate::host::HostProcessError::Io(std::io::Error::other(
        "injected host resource preparation failure",
    ))
}

#[test]
fn cancellation_wins_a_backend_prepare_error() {
    let result = cancellation_wins_pre_spawn::<()>(
        &TaskContext::detached_cancelled(),
        Err(TaskError::fatal("injected backend preparation error")),
    );

    assert!(matches!(result, Err(TaskError::Canceled)));
}

#[test]
fn cancellation_wins_a_script_materialization_error() {
    let result = cancellation_wins_pre_spawn::<()>(
        &TaskContext::detached_cancelled(),
        Err(TaskError::fail("injected script materialization error")),
    );

    assert!(matches!(result, Err(TaskError::Canceled)));
}

#[test]
fn active_backend_prepare_error_is_preserved() {
    let result = cancellation_wins_pre_spawn::<()>(
        &TaskContext::detached(),
        Err(TaskError::fatal("injected backend preparation error").with_exit_code(71)),
    );

    match result.unwrap_err() {
        TaskError::Fatal {
            reason, exit_code, ..
        } => {
            assert_eq!(reason, "injected backend preparation error");
            assert_eq!(exit_code, Some(71));
        }
        other => panic!("expected preserved fatal backend error, got {other:?}"),
    }
}

#[test]
fn active_script_materialization_error_is_preserved() {
    let result = cancellation_wins_pre_spawn::<()>(
        &TaskContext::detached(),
        Err(TaskError::fail("injected script materialization error").with_exit_code(72)),
    );

    match result.unwrap_err() {
        TaskError::Fail {
            reason, exit_code, ..
        } => {
            assert_eq!(reason, "injected script materialization error");
            assert_eq!(exit_code, Some(72));
        }
        other => panic!("expected preserved script error, got {other:?}"),
    }
}

#[test]
fn cancellation_drops_a_successful_pre_spawn_result() {
    struct DropProbe(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = cancellation_wins_pre_spawn(
        &TaskContext::detached_cancelled(),
        Ok(DropProbe(Arc::clone(&dropped))),
    );

    assert!(matches!(result, Err(TaskError::Canceled)));
    assert!(dropped.load(Ordering::Acquire));
}

fn expect_injected_prepare_error(prepared: Result<PreparedProcessOwnership, crate::ExecError>) {
    let error = match prepared {
        Ok(_) => panic!("injected preparation failure must be returned"),
        Err(error) => error,
    };
    let crate::ExecError::Io(error) = error else {
        panic!("injected I/O failure must preserve its public error kind");
    };
    assert_eq!(
        error.to_string(),
        "injected host resource preparation failure"
    );
}

#[tokio::test]
async fn clean_prepare_failure_releases_admission() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    expect_injected_prepare_error(finish_host_process_prepare(
        Err(crate::host::AttemptPrepareFailure::Clean(
            injected_prepare_error(),
        )),
        finalizer.try_reserve().unwrap(),
    ));

    let status = finalizer.status();
    assert_eq!(status.owned(), 0);
    assert!(status.healthy());
    assert!(status.accepting());
    finalizer.shutdown(StdDuration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn pinned_prepare_residual_is_finalized_before_admission_releases() {
    let parent = tempfile::TempDir::new().unwrap();
    let cgroup = parent.path().join("injected-pinned-residual");
    std::fs::create_dir(&cgroup).unwrap();
    let blocker = cgroup.join("cleanup-blocker");
    std::fs::write(&blocker, b"owned").unwrap();
    let cleanup = crate::host::AttemptProcessDomain::for_test(cgroup.clone());
    let finalizer = DropFinalizerDomain::start(1).unwrap();

    expect_injected_prepare_error(finish_host_process_prepare(
        Err(crate::host::AttemptPrepareFailure::Residual {
            error: injected_prepare_error(),
            cleanup: Some(cleanup),
        }),
        finalizer.try_reserve().unwrap(),
    ));

    assert!(cgroup.exists());
    assert_eq!(finalizer.status().owned(), 1);
    let full = match finalizer.try_reserve() {
        Ok(_) => panic!("pinned residual must remain charged through cleanup"),
        Err(error) => error,
    };
    assert_eq!(full.kind(), std::io::ErrorKind::WouldBlock);

    std::fs::remove_file(blocker).unwrap();
    wait_for_finalizer_release(&finalizer).await;
    assert!(!cgroup.exists());
    let status = finalizer.status();
    assert!(status.healthy());
    assert!(status.accepting());
    finalizer.shutdown(StdDuration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn unpinned_prepare_residual_terminally_quarantines_admission() {
    let finalizer = DropFinalizerDomain::start(1).unwrap();
    expect_injected_prepare_error(finish_host_process_prepare(
        Err(crate::host::AttemptPrepareFailure::Residual {
            error: injected_prepare_error(),
            cleanup: None,
        }),
        finalizer.try_reserve().unwrap(),
    ));

    let immediate = finalizer.status();
    assert_eq!(immediate.owned(), 1);
    assert!(!immediate.healthy());
    assert!(!immediate.accepting());
    let unavailable = match finalizer.try_reserve() {
        Ok(_) => panic!("unrecoverable residual must close admission immediately"),
        Err(error) => error,
    };
    assert_eq!(unavailable.kind(), std::io::ErrorKind::BrokenPipe);

    tokio::time::timeout(StdDuration::from_secs(2), async {
        while finalizer.status().quarantined() != 1 {
            tokio::time::sleep(StdDuration::from_millis(1)).await;
        }
    })
    .await
    .expect("unrecoverable preparation ownership was not quarantined");

    let status = finalizer.status();
    assert_eq!(status.owned(), 1);
    assert!(!status.healthy());
    assert!(!status.accepting());
    let shutdown = finalizer
        .shutdown(StdDuration::from_secs(2))
        .await
        .unwrap_err();
    assert_eq!(shutdown.kind(), std::io::ErrorKind::Other);
}

#[test]
fn canceled_script_materialization_retains_prepared_ownership() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (release, blocker) = occupy_only_blocking_worker().await;
            let runner = SubprocessRunner::new("script-cancel-test").unwrap();
            let backend = Arc::clone(&runner.config);
            let metrics = BuildContext::default().metrics().clone();
            let finalizer = DropFinalizerDomain::start(1).unwrap();
            let prepared = backend.prepare_host_process_attempt(None).unwrap();
            let ownership =
                PreparedProcessOwnership::new(prepared, finalizer.try_reserve().unwrap());
            let mut materialization =
                Box::pin(materialize_script(&metrics, Arc::from("exit 0"), ownership));

            assert_future_pending(materialization.as_mut());
            drop(materialization);
            assert_eq!(finalizer.status().owned(), 1);
            let full = match finalizer.try_reserve() {
                Ok(_) => panic!("queued script ownership must retain admission"),
                Err(error) => error,
            };
            assert_eq!(full.kind(), std::io::ErrorKind::WouldBlock);

            release.send(()).unwrap();
            blocker.await.unwrap();
            wait_for_finalizer_release(&finalizer).await;
            finalizer.shutdown(StdDuration::from_secs(2)).await.unwrap();
            runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
        });
}

#[test]
fn canceled_cgroup_preparation_retains_reserved_ownership() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (release, blocker) = occupy_only_blocking_worker().await;
            let runner = SubprocessRunner::new("cgroup-cancel-test").unwrap();
            let backend = Arc::clone(&runner.config);
            let metrics = BuildContext::default().metrics().clone();
            let finalizer = DropFinalizerDomain::start(1).unwrap();
            let reservation = finalizer.try_reserve().unwrap();
            let mut preparation = Box::pin(prepare_backend(
                &backend,
                &metrics,
                Some("queued-cgroup".into()),
                reservation,
            ));

            assert_future_pending(preparation.as_mut());
            drop(preparation);
            assert_eq!(finalizer.status().owned(), 1);
            let full = match finalizer.try_reserve() {
                Ok(_) => panic!("queued cgroup ownership must retain admission"),
                Err(error) => error,
            };
            assert_eq!(full.kind(), std::io::ErrorKind::WouldBlock);

            release.send(()).unwrap();
            blocker.await.unwrap();
            wait_for_finalizer_release(&finalizer).await;
            finalizer.shutdown(StdDuration::from_secs(2)).await.unwrap();
            runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
        });
}

type SinkCalls = Arc<std::sync::Mutex<Vec<(solti_model::TaskId, u64, u32)>>>;

struct RecordingOutputPublisher {
    sender: std::sync::mpsc::Sender<solti_model::OutputEvent>,
    calls: SinkCalls,
}

impl solti_runner::OutputPublisher for RecordingOutputPublisher {
    fn sink_for(
        &self,
        task_name: &solti_model::TaskId,
        generation: u64,
        attempt: u32,
    ) -> Option<solti_runner::OutputSink> {
        self.calls
            .lock()
            .unwrap()
            .push((task_name.clone(), generation, attempt));
        let sender = self.sender.clone();
        Some(solti_runner::OutputSink::new(
            generation,
            attempt,
            move |event| {
                let _ = sender.send(event);
            },
        ))
    }
}

fn recording_output_context() -> (
    BuildContext,
    std::sync::mpsc::Receiver<solti_model::OutputEvent>,
    SinkCalls,
) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let publisher: solti_runner::OutputPublisherHandle = Arc::new(RecordingOutputPublisher {
        sender,
        calls: Arc::clone(&calls),
    });
    (
        BuildContext::default().with_output_publisher(publisher),
        receiver,
        calls,
    )
}

#[cfg(unix)]
async fn wait_for_recorded_pid(marker: &std::path::Path) -> Option<i32> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(marker)
                && let Some(line) = value.trim().lines().next()
                && let Ok(pid) = line.parse()
            {
                break pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .ok()
}

#[cfg(unix)]
async fn assert_process_gone(pid: i32) {
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            // SAFETY: signal zero performs an existence check and `pid` was
            // reported by the child process started by this test.
            if unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();

    if !stopped {
        // SAFETY: `pid` was reported by the child process started by this test;
        // failure is intentionally ignored before the panic.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        panic!("descendant process {pid} survived cleanup");
    }
}

fn mk_backoff() -> solti_model::BackoffPolicy {
    solti_model::BackoffPolicy {
        jitter: solti_model::JitterPolicy::Equal,
        first_ms: 100,
        max_ms: 1000,
        factor: 2.0,
    }
}

fn mk_subprocess_spec(slot: &str, command: &str) -> Task {
    mk_subprocess_spec_with_args(slot, command, &[])
}

fn mk_subprocess_spec_with_args(slot: &str, command: &str, args: &[&str]) -> Task {
    mk_command_spec(slot, command, args, Default::default(), None)
}

fn mk_command_spec(
    slot: &str,
    command: &str,
    args: &[&str],
    env: solti_model::TaskEnv,
    cwd: Option<std::path::PathBuf>,
) -> Task {
    let spec = solti_model::TaskSpec::builder(
        slot,
        TaskWorkload::Subprocess(SubprocessSpec::new(
            solti_model::SubprocessMode::Command {
                command: command.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
            },
            env,
            cwd,
            Default::default(),
        )),
        5_000u64,
    )
    .restart(solti_model::RestartPolicy::Never)
    .backoff(mk_backoff())
    .admission(solti_model::AdmissionPolicy::DropIfRunning)
    .build()
    .unwrap();
    Task::new(format!("task-{slot}"), spec).unwrap()
}

fn mk_script_spec(slot: &str, body: &[u8], args: &[&str]) -> Task {
    mk_script_spec_with_interpreter(slot, "bash", body, args)
}

fn mk_script_spec_with_interpreter(
    slot: &str,
    interpreter: &str,
    body: &[u8],
    args: &[&str],
) -> Task {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let spec = solti_model::TaskSpec::builder(
        slot,
        TaskWorkload::Subprocess(SubprocessSpec::new(
            solti_model::SubprocessMode::Script {
                interpreter: interpreter.into(),
                body: BASE64.encode(body),
                args: args.iter().map(|s| s.to_string()).collect(),
            },
            Default::default(),
            None,
            Default::default(),
        )),
        5_000u64,
    )
    .restart(solti_model::RestartPolicy::Never)
    .backoff(mk_backoff())
    .admission(solti_model::AdmissionPolicy::DropIfRunning)
    .build()
    .unwrap();
    Task::new(format!("task-{slot}"), spec).unwrap()
}

fn mk_embedded_spec(slot: &str) -> Task {
    let workload = TaskWorkload::Embedded(
        solti_model::EmbeddedSpec::new("test-revision").expect("valid embedded revision"),
    );
    let spec = solti_model::TaskSpec::builder(slot, workload, 5_000u64)
        .restart(solti_model::RestartPolicy::Never)
        .backoff(mk_backoff())
        .admission(solti_model::AdmissionPolicy::DropIfRunning)
        .build()
        .unwrap();
    Task::new(format!("task-{slot}"), spec).unwrap()
}

async fn build_with_run_id(
    runner: &SubprocessRunner,
    task: &Task,
    ctx: &BuildContext,
) -> Result<TaskRef, RunnerError> {
    let run_id = solti_runner::make_run_id(runner.name(), task.slot().as_str());
    let mut scope = solti_runner::BuildScope::unmanaged(runner.name());
    runner
        .build_task(
            task,
            &run_id,
            ctx,
            &solti_runner::BuildCancellation::new(),
            &mut scope,
        )
        .await
}

#[test]
fn cwd_pinning_does_not_use_tokio_blocking_workers() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (release, blocker) = occupy_only_blocking_worker().await;
            let directory = tempfile::TempDir::new().unwrap();
            let spec = solti_model::TaskSpec::builder(
                "dedicated-cwd",
                TaskWorkload::Subprocess(SubprocessSpec::new(
                    solti_model::SubprocessMode::Command {
                        command: "true".into(),
                        args: Vec::new(),
                    },
                    Default::default(),
                    Some(directory.path().to_path_buf()),
                    Default::default(),
                )),
                5_000_u64,
            )
            .build()
            .unwrap();
            let task = Task::new("dedicated-cwd", spec).unwrap();
            let runner = SubprocessRunner::new("dedicated-cwd").unwrap();

            tokio::time::timeout(
                StdDuration::from_secs(1),
                build_with_run_id(&runner, &task, &BuildContext::default()),
            )
            .await
            .expect("cwd pinning must not wait for Tokio's blocking pool")
            .unwrap();

            release.send(()).unwrap();
            blocker.await.unwrap();
            runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
        });
}

#[tokio::test]
async fn pre_cancelled_cwd_build_returns_typed_cancellation() {
    let runner = SubprocessRunner::new("cancelled-cwd").unwrap();
    let task = mk_subprocess_spec("cancelled-cwd", "true");
    let run_id = solti_runner::make_run_id(runner.name(), task.slot().as_str());
    let mut scope = solti_runner::BuildScope::unmanaged(runner.name());
    let (cancel, cancellation) = solti_runner::BuildCancellation::pair();
    cancel.cancel();

    let error = match runner
        .build_task(
            &task,
            &run_id,
            &BuildContext::default(),
            &cancellation,
            &mut scope,
        )
        .await
    {
        Ok(_) => panic!("pre-cancelled build unexpectedly produced a task"),
        Err(error) => error,
    };

    assert!(matches!(error, RunnerError::BuildCancelled));
    runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
}

#[tokio::test]
async fn output_drain_preserves_reader_join_failures() {
    let mut output = OutputTasks::new();
    output.spawn_stdout(async { panic!("stdout reader failure") });
    output.spawn_stderr(async {});

    let drain = output.drain().await;

    assert!(matches!(
        drain,
        OutputDrain::Completed {
            stdout: Some(OutputReaderFailure::Panicked),
            stderr: None,
        }
    ));
}

fn make_task_cfg() -> SubprocessTaskConfig {
    SubprocessTaskConfig {
        run_id: Arc::from("test-run-1"),
        seq: 1,
        command: "echo".into(),
        args: vec!["hello".into()],
        env: Default::default(),
        cwd: None,
        fail_on_non_zero: solti_model::Flag::default(),
    }
}

fn make_exec_ctx() -> TaskExecContext {
    TaskExecContext {
        task_cfg: make_task_cfg(),
        runner_cfg: Arc::new(SubprocessBackendConfig::new().prepare().unwrap()),
        cgroup_name: None,
        metrics: solti_runner::noop_metrics(),
        log_cfg: LogConfig::default(),
        output_publisher: solti_runner::noop_output_publisher(),
        attempt: AtomicU32::new(0),
        generation: 1,
        resource_name: solti_model::TaskId::new("test-resource").unwrap(),
        finalizer: DropFinalizerDomain::start(8).unwrap(),
        script_body: None,
        pinned_cwd: None,
    }
}

#[test]
fn build_command_sets_args_and_pipes() {
    let ctx = make_exec_ctx();
    let cmd = build_command(&ctx, None);
    let std_cmd = cmd.as_std();
    assert_eq!(std_cmd.get_program(), "echo");
    let args: Vec<_> = std_cmd.get_args().collect();
    assert_eq!(args, vec!["hello"]);
}

#[cfg(target_os = "macos")]
async fn run_through_macos_fallback(mut ctx: TaskExecContext) {
    let environment = unix_child_environment(&ctx);
    let prepared = ctx.runner_cfg.prepare_host_process_attempt(None).unwrap();
    let prepared = match try_spawn_macos(&ctx, None, prepared, &environment).unwrap() {
        MacosSpawnAttempt::Fallback { prepared, .. } => prepared,
        MacosSpawnAttempt::Spawned(_, _) => panic!("test case unexpectedly used native spawn"),
    };
    ctx.task_cfg
        .env
        .insert("SOLTI_FALLBACK_SNAPSHOT".into(), "after-snapshot".into());
    let reservation = ctx.finalizer.try_reserve().unwrap();
    let prepared = PreparedProcessOwnership::new(prepared, reservation);
    let (mut child, _domain, _reservation) =
        spawn_with_command(&ctx, None, prepared, &environment).unwrap();
    assert!(child.wait().await.unwrap().success());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_macos_spawn_rejects_executable_text_without_shebang() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::TempDir::new().unwrap();
    let program = directory.path().join("plain-text");
    std::fs::write(&program, b"exit 0\n").unwrap();
    let mut permissions = std::fs::metadata(&program).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&program, permissions).unwrap();

    let mut ctx = make_exec_ctx();
    ctx.task_cfg.command = program.to_string_lossy().into_owned();
    ctx.task_cfg.args.clear();
    let environment = unix_child_environment(&ctx);
    let prepared = ctx.runner_cfg.prepare_host_process_attempt(None).unwrap();
    let error = match try_spawn_macos(&ctx, None, prepared, &environment) {
        Ok(_) => panic!("native macOS spawn accepted executable text without a shebang"),
        Err(error) => error,
    };
    assert!(matches!(error, TaskError::Fatal { .. }));
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, body: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
async fn assert_command_is_permanently_rejected(task: &Task, runner_name: &str) {
    let runner = SubprocessRunner::new(runner_name).unwrap();
    let task = build_with_run_id(&runner, task, &BuildContext::default())
        .await
        .unwrap();
    match task.spawn(TaskContext::detached()).await {
        Err(TaskError::Fatal { reason, .. }) => {
            assert!(reason.contains("spawn failed"), "{reason}");
            assert!(
                reason.contains(&format!("os error {}", libc::ENOEXEC)),
                "{reason}"
            );
        }
        other => panic!("expected permanent spawn failure, got {other:?}"),
    }
    runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
}

#[cfg(target_os = "macos")]
async fn assert_command_succeeds(task: &Task, runner_name: &str) {
    let runner = SubprocessRunner::new(runner_name).unwrap();
    let task = build_with_run_id(&runner, task, &BuildContext::default())
        .await
        .unwrap();
    task.spawn(TaskContext::detached()).await.unwrap();
    runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn command_rejects_absolute_executable_text_without_shebang() {
    let directory = tempfile::TempDir::new().unwrap();
    let program = directory.path().join("plain-absolute");
    write_executable(&program, b"exit 0\n");
    let task = mk_subprocess_spec("plain-absolute", program.to_str().unwrap());

    assert_command_is_permanently_rejected(&task, "plain-absolute").await;
}

#[cfg(unix)]
#[tokio::test]
async fn command_rejects_relative_executable_text_in_pinned_cwd() {
    let directory = tempfile::TempDir::new().unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    write_executable(&canonical.join("plain-relative"), b"exit 0\n");
    let task = mk_command_spec(
        "plain-relative",
        "./plain-relative",
        &[],
        Default::default(),
        Some(canonical),
    );

    assert_command_is_permanently_rejected(&task, "plain-relative").await;
}

#[cfg(unix)]
#[tokio::test]
async fn command_rejects_path_resolved_executable_text_without_shebang() {
    let directory = tempfile::TempDir::new().unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    write_executable(&canonical.join("plain-path"), b"exit 0\n");
    let task = mk_command_spec(
        "plain-path",
        "plain-path",
        &[],
        solti_model::TaskEnv::single("PATH", canonical.to_str().unwrap()),
        None,
    );

    assert_command_is_permanently_rejected(&task, "plain-path").await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_fork_fallback_rejects_executable_text_without_shebang() {
    let directory = tempfile::TempDir::new().unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    std::fs::create_dir(canonical.join("bin")).unwrap();
    write_executable(&canonical.join("bin/plain-fallback"), b"exit 0\n");
    let task = mk_command_spec(
        "plain-fallback",
        "plain-fallback",
        &[],
        solti_model::TaskEnv::single("PATH", "bin"),
        Some(canonical),
    );

    assert_command_is_permanently_rejected(&task, "plain-fallback").await;
}

#[cfg(unix)]
#[tokio::test]
async fn command_preserves_binary_and_kernel_shebang_execution() {
    let binary_runner = SubprocessRunner::new("direct-binary").unwrap();
    let binary = mk_subprocess_spec("direct-binary", "/usr/bin/true");
    let binary = build_with_run_id(&binary_runner, &binary, &BuildContext::default())
        .await
        .unwrap();
    binary.spawn(TaskContext::detached()).await.unwrap();
    binary_runner
        .shutdown(StdDuration::from_secs(2))
        .await
        .unwrap();

    let directory = tempfile::TempDir::new().unwrap();
    let program = directory.path().join("shebang-command");
    write_executable(
        &program,
        b"#!/bin/sh\nif [ \"$1\" != 'literal;exit 41' ] || [ \"$SOLTI_LITERAL\" != 'env;exit 42' ]; then exit 97; fi\nexit 0\n",
    );
    let shebang = mk_command_spec(
        "direct-shebang",
        program.to_str().unwrap(),
        &["literal;exit 41"],
        solti_model::TaskEnv::single("SOLTI_LITERAL", "env;exit 42"),
        None,
    );
    let shebang_runner = SubprocessRunner::new("direct-shebang").unwrap();
    let shebang = build_with_run_id(&shebang_runner, &shebang, &BuildContext::default())
        .await
        .unwrap();
    shebang.spawn(TaskContext::detached()).await.unwrap();
    shebang_runner
        .shutdown(StdDuration::from_secs(2))
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_path_skips_a_symlink_loop_before_a_valid_executable() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::TempDir::new().unwrap();
    let first = directory.path().join("loop");
    let second = directory.path().join("valid");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    symlink("probe", first.join("probe")).unwrap();
    write_executable(&second.join("probe"), b"#!/bin/sh\nexit 0\n");
    let path = std::env::join_paths([first, second])
        .unwrap()
        .into_string()
        .unwrap();
    let task = mk_command_spec(
        "path-loop",
        "probe",
        &[],
        solti_model::TaskEnv::single("PATH", path),
        None,
    );

    assert_command_succeeds(&task, "path-loop").await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_path_skips_an_overlong_component_before_a_valid_executable() {
    let directory = tempfile::TempDir::new().unwrap();
    let valid = directory.path().join("valid");
    std::fs::create_dir(&valid).unwrap();
    write_executable(&valid.join("probe"), b"#!/bin/sh\nexit 0\n");
    let path = format!(
        "{}:{}",
        "x".repeat(libc::PATH_MAX as usize),
        valid.display()
    );
    let task = mk_command_spec(
        "path-long",
        "probe",
        &[],
        solti_model::TaskEnv::single("PATH", path),
        None,
    );

    assert_command_succeeds(&task, "path-long").await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn relative_child_path_in_pinned_cwd_runs_through_fork_fallback() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::TempDir::new().unwrap();
    let canonical = directory.path().canonicalize().unwrap();
    std::fs::create_dir(canonical.join("bin")).unwrap();
    let program = canonical.join("bin/probe");
    std::fs::write(
        &program,
        b"#!/bin/sh\ntest \"$SOLTI_FALLBACK_SNAPSHOT\" = before-snapshot\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&program).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&program, permissions).unwrap();

    let mut ctx = make_exec_ctx();
    ctx.task_cfg.command = "probe".into();
    ctx.task_cfg.args.clear();
    ctx.task_cfg.env.insert("PATH".into(), "bin".into());
    ctx.task_cfg
        .env
        .insert("SOLTI_FALLBACK_SNAPSHOT".into(), "before-snapshot".into());
    ctx.pinned_cwd = Some(PinnedCwd::open_absolute(&canonical).unwrap());
    run_through_macos_fallback(ctx).await;
}

#[cfg(unix)]
#[tokio::test]
async fn default_subprocess_owns_a_session() {
    let mut ctx = make_exec_ctx();
    ctx.task_cfg.command = "sleep".into();
    ctx.task_cfg.args = vec!["30".into()];
    let mut command = build_command(&ctx, None);
    let mut child = command.spawn().unwrap();
    let pid = child.id().unwrap() as libc::pid_t;

    // SAFETY: `getsid` only reads process metadata for a numeric pid.
    assert_eq!(unsafe { libc::getsid(pid) }, pid);

    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[cfg(unix)]
#[test]
fn child_inherits_only_explicitly_passed_fd() {
    use std::os::fd::AsRawFd as _;

    let denied_file = tempfile::tempfile().unwrap();
    let denied_fd = denied_file.as_raw_fd();
    let mut denied_ctx = make_exec_ctx();
    denied_ctx.task_cfg.command = "test".into();
    denied_ctx.task_cfg.args = vec!["-e".into(), format!("/dev/fd/{denied_fd}")];
    let mut denied = build_command(&denied_ctx, None);
    apply_fd_boundary(&mut denied, &denied_ctx, None).unwrap();
    assert!(!denied.as_std_mut().status().unwrap().success());

    let passed_file = tempfile::tempfile().unwrap();
    let passed_fd = passed_file.as_raw_fd();
    let mut passed_ctx =
        ctx_with_backend(SubprocessBackendConfig::new().with_passed_fd(passed_file.into()));
    passed_ctx.task_cfg.command = "test".into();
    passed_ctx.task_cfg.args = vec!["-e".into(), format!("/dev/fd/{passed_fd}")];
    let mut passed = build_command(&passed_ctx, None);
    apply_fd_boundary(&mut passed, &passed_ctx, None).unwrap();
    assert!(passed.as_std_mut().status().unwrap().success());
}

#[cfg(unix)]
#[tokio::test]
async fn new_session_policy_does_not_also_request_process_group() {
    use crate::host::{HostProcessPolicy, ProcessConfig};

    let mut ctx = ctx_with_backend(SubprocessBackendConfig::new().with_host_process_policy(
        HostProcessPolicy::new().with_process_config(ProcessConfig {
            new_session: true,
            ..Default::default()
        }),
    ));
    ctx.task_cfg.command = "sh".into();
    ctx.task_cfg.args = vec!["-c".into(), "exit 0".into()];

    let prepared = ctx.runner_cfg.prepare_host_process_attempt(None).unwrap();
    let mut command = build_command(&ctx, None);
    apply_fd_boundary(&mut command, &ctx, None).unwrap();
    let _guard = apply_backend(&mut command, &ctx, prepared);

    assert!(command.status().await.unwrap().success());
}

#[test]
fn runner_name_validation_accepts_and_rejects() {
    for good in ["subprocess", "runner-1", "a.b_c", "x"] {
        assert!(validate_runner_name(good).is_ok(), "should accept {good:?}");
    }
    for bad in ["", ".", "..", "a/b", "a b", "runner\0", &"n".repeat(65)] {
        assert!(validate_runner_name(bad).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn with_config_rejects_bad_runner_name() {
    let result = SubprocessRunner::with_config("bad/name", SubprocessBackendConfig::new());
    let err = result.err().expect("bad name must be rejected").to_string();
    assert!(err.contains("invalid runner name"), "got: {err}");
}

#[test]
fn runner_accepts_a_dynamically_owned_name() {
    let suffix = 7;
    let name = format!("runner-{suffix}");
    let runner = SubprocessRunner::new(name.clone()).unwrap();

    assert_eq!(runner.name(), name);
}

#[test]
fn build_command_prepends_script_path() {
    let ctx = make_exec_ctx();
    let cmd = build_command(&ctx, Some(std::path::Path::new("/tmp/solti-script-x")));
    let args: Vec<_> = cmd.as_std().get_args().collect();
    assert_eq!(args, vec!["/tmp/solti-script-x", "hello"]);
}

#[cfg(not(unix))]
fn env_of(cmd: &Command) -> std::collections::HashMap<String, Option<String>> {
    cmd.as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect()
}

fn ctx_with_backend(cfg: SubprocessBackendConfig) -> TaskExecContext {
    let mut ctx = make_exec_ctx();
    ctx.runner_cfg = Arc::new(cfg.prepare().unwrap());
    ctx
}

#[cfg(not(unix))]
#[test]
fn env_inherit_injects_no_path() {
    use crate::subprocess::backend::EnvPolicy;

    let ctx = ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Inherit));
    let cmd = build_command(&ctx, None);
    assert!(!env_of(&cmd).contains_key("PATH"));
}

#[cfg(not(unix))]
#[test]
fn env_clear_injects_safe_path() {
    use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};
    let ctx = ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
    let cmd = build_command(&ctx, None);
    let env = env_of(&cmd);
    assert_eq!(env.get("PATH"), Some(&Some(SAFE_DEFAULT_PATH.to_string())));
}

#[cfg(not(unix))]
#[test]
fn env_clear_respects_task_provided_path() {
    use crate::subprocess::backend::EnvPolicy;
    let mut ctx =
        ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
    ctx.task_cfg
        .env
        .insert("PATH".into(), "/opt/custom/bin".into());
    let cmd = build_command(&ctx, None);
    assert_eq!(
        env_of(&cmd).get("PATH"),
        Some(&Some("/opt/custom/bin".to_string()))
    );
}

#[cfg(not(unix))]
#[test]
fn env_clear_keeps_task_vars() {
    use crate::subprocess::backend::EnvPolicy;
    let mut ctx =
        ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
    ctx.task_cfg.env.insert("FOO".into(), "bar".into());
    let cmd = build_command(&ctx, None);
    assert_eq!(env_of(&cmd).get("FOO"), Some(&Some("bar".to_string())));
}

#[cfg(not(unix))]
#[test]
fn env_allowlist_skips_absent_key_and_still_injects_path() {
    use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};
    // An allowlisted var that is not in the parent env is simply skipped;
    // PATH is still injected because neither the task nor the allowlist set it.
    let ctx = ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(
        EnvPolicy::Allowlist(vec!["SOLTI_DEFINITELY_ABSENT_VAR_XYZ".into()]),
    ));
    let cmd = build_command(&ctx, None);
    assert_eq!(
        env_of(&cmd).get("PATH"),
        Some(&Some(SAFE_DEFAULT_PATH.to_string()))
    );
}

#[cfg(unix)]
#[test]
fn unix_build_command_defers_environment_materialization() {
    use crate::subprocess::backend::EnvPolicy;

    let ctx = ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
    let cmd = build_command(&ctx, None);

    assert_eq!(cmd.as_std().get_envs().count(), 0);
}

#[cfg(unix)]
#[test]
fn unix_child_environment_materializes_inherit_once() {
    use crate::subprocess::backend::EnvPolicy;

    let mut ctx =
        ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Inherit));
    ctx.task_cfg
        .env
        .insert("SOLTI_TASK_ENV".into(), "task".into());
    let mut expected: std::collections::BTreeMap<OsString, OsString> =
        std::env::vars_os().collect();
    expected.extend(
        ctx.task_cfg
            .env
            .iter()
            .map(|(key, value)| (key.into(), value.into())),
    );

    assert_eq!(unix_child_environment(&ctx), expected);
}

#[cfg(unix)]
#[test]
fn unix_child_environment_applies_clear_and_allowlist() {
    use crate::subprocess::backend::{EnvPolicy, SAFE_DEFAULT_PATH};

    let mut clear =
        ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(EnvPolicy::Clear));
    clear
        .task_cfg
        .env
        .insert("SOLTI_TASK_ENV".into(), "task".into());
    let clear_environment = unix_child_environment(&clear);
    assert_eq!(
        clear_environment.get(OsStr::new("PATH")),
        Some(&OsString::from(SAFE_DEFAULT_PATH))
    );
    assert_eq!(
        clear_environment.get(OsStr::new("SOLTI_TASK_ENV")),
        Some(&OsString::from("task"))
    );

    let allowlist = ctx_with_backend(SubprocessBackendConfig::new().with_env_policy(
        EnvPolicy::Allowlist(vec!["SOLTI_DEFINITELY_ABSENT_VAR_XYZ".into()]),
    ));
    let allowlist_environment = unix_child_environment(&allowlist);
    assert_eq!(
        allowlist_environment.get(OsStr::new("PATH")),
        Some(&OsString::from(SAFE_DEFAULT_PATH))
    );
    assert!(!allowlist_environment.contains_key(OsStr::new("SOLTI_DEFINITELY_ABSENT_VAR_XYZ")));
}

#[test]
fn evaluate_exit_respects_fail_on_non_zero() {
    use std::process::Command as StdCommand;

    let success = StdCommand::new("true").status().unwrap();
    let failed = StdCommand::new("false").status().unwrap();
    let mut cfg = make_task_cfg();
    assert!(evaluate_exit(success, &cfg).is_ok());

    cfg.fail_on_non_zero = solti_model::Flag::disabled();
    assert!(evaluate_exit(failed, &cfg).is_ok());

    cfg.fail_on_non_zero = solti_model::Flag::enabled();
    let result = evaluate_exit(failed, &cfg);
    assert!(result.is_err());
    match result.unwrap_err() {
        TaskError::Fail {
            reason, exit_code, ..
        } => {
            assert!(reason.contains("non-zero"));
            assert_eq!(exit_code, Some(1));
        }
        other => panic!("expected TaskError::Fail, got {other:?}"),
    }
}

#[tokio::test]
async fn build_task_returns_runnable_subprocess() {
    let runner = SubprocessRunner::new("test-runner").unwrap();
    let task = mk_subprocess_spec("test-slot", "echo");
    let task_ref = build_with_run_id(&runner, &task, &BuildContext::default())
        .await
        .unwrap();

    task_ref
        .spawn(TaskContext::detached())
        .await
        .expect("the built subprocess must run");
}

#[tokio::test]
async fn pre_cancelled_subprocess_requests_no_sink_or_cleanup_ownership() {
    let (build, _events, calls) = recording_output_context();
    let runner = SubprocessRunner::new("pre-cancel-runner").unwrap();
    let task = mk_subprocess_spec("pre-cancel-slot", "/definitely/not/a/solti-command");
    let task_ref = build_with_run_id(&runner, &task, &build).await.unwrap();

    let result = task_ref.spawn(TaskContext::detached_cancelled()).await;

    assert!(matches!(result, Err(TaskError::Canceled)));
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(runner.finalizer_status().owned(), 0);
    runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
}

#[test]
fn cancellation_after_awaited_script_prepare_prevents_spawn() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let (release, blocker) = occupy_only_blocking_worker().await;
            let (build, _events, calls) = recording_output_context();
            let runner = SubprocessRunner::new("prepare-cancel-runner").unwrap();
            let task = mk_script_spec_with_interpreter(
                "prepare-cancel-slot",
                "/definitely/not/a/solti-interpreter",
                b"exit 0",
                &[],
            );
            let task_ref = build_with_run_id(&runner, &task, &build).await.unwrap();
            let supervisor =
                taskvisor::Supervisor::new(taskvisor::SupervisorConfig::default(), Vec::new());
            let handle = supervisor.serve().unwrap();
            let (id, waiter) = handle
                .add_and_watch(taskvisor::TaskSpec::once(
                    "prepare-cancel-attempt",
                    task_ref,
                ))
                .await
                .unwrap();

            tokio::time::timeout(StdDuration::from_secs(2), async {
                while calls.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("attempt did not reach script preparation");
            assert_eq!(runner.finalizer_status().owned(), 1);
            assert!(handle.remove(id).await.unwrap());

            release.send(()).unwrap();
            blocker.await.unwrap();

            let outcome = waiter.wait().await.unwrap();
            assert_eq!(outcome.kind(), taskvisor::TaskOutcomeKind::Canceled);
            wait_for_finalizer_release(&runner.finalizer).await;
            runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
            handle.shutdown().await.unwrap();
        });
}

#[tokio::test]
async fn tracing_does_not_record_subprocess_process_inputs() {
    const COMMAND_SECRET: &str = "subprocess-command-secret";
    const ARG_SECRET: &str = "subprocess-argument-secret";
    const ENV_SECRET: &str = "subprocess-environment-secret";
    const FORGED: &str = "forged-subprocess-record";

    let runner = SubprocessRunner::new("trace-runner").unwrap();
    let capture = Arc::new(TraceCapture::default());
    let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
    let _interest_guard = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));

    let warmup = mk_subprocess_spec("trace-warmup", "solti-missing-safe-warmup-command");
    let warmup = build_with_run_id(&runner, &warmup, &BuildContext::default())
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    assert!(
        warmup
            .spawn(TaskContext::detached())
            .with_subscriber(dispatch.clone())
            .await
            .is_err()
    );
    tracing::dispatcher::with_default(&dispatch, tracing::callsite::rebuild_interest_cache);
    capture.fields.lock().unwrap().clear();

    let spec = solti_model::TaskSpec::builder(
        "trace-secret",
        TaskWorkload::Subprocess(SubprocessSpec::new(
            solti_model::SubprocessMode::Command {
                command: format!("https://user:{COMMAND_SECRET}@host.invalid/tool\n{FORGED}"),
                args: vec![format!("--token={ARG_SECRET}")],
            },
            solti_model::TaskEnv::single("SECRET_TOKEN", ENV_SECRET),
            None,
            Default::default(),
        )),
        5_000u64,
    )
    .restart(solti_model::RestartPolicy::Never)
    .build()
    .unwrap();
    let resource = Task::new("task-trace-secret", spec).unwrap();
    let task = build_with_run_id(&runner, &resource, &BuildContext::default())
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    let error = task
        .spawn(TaskContext::detached())
        .with_subscriber(dispatch)
        .await
        .unwrap_err();

    let fields = capture.fields.lock().unwrap().join(" ");
    let error = format!("{error:?} {error}");
    assert!(fields.contains("subprocess.lifecycle"), "{fields}");
    assert!(fields.contains("spawning"), "{fields}");
    assert!(fields.contains("arg_count"), "{fields}");
    assert!(!fields.contains("command="), "{fields}");
    assert!(!fields.contains(COMMAND_SECRET), "{fields}");
    assert!(!fields.contains(ARG_SECRET), "{fields}");
    assert!(!fields.contains(ENV_SECRET), "{fields}");
    assert!(!fields.contains(FORGED), "{fields}");
    for secret in [COMMAND_SECRET, ARG_SECRET, ENV_SECRET, FORGED] {
        assert!(!error.contains(secret), "{error}");
    }
}

#[tokio::test]
async fn build_task_rejects_non_subprocess_kind() {
    let runner = SubprocessRunner::new("test-runner").unwrap();
    let spec = mk_embedded_spec("test-slot");
    match build_with_run_id(&runner, &spec, &BuildContext::default()).await {
        Err(RunnerError::UnsupportedWorkload {
            runner,
            api_version,
            kind,
        }) => {
            assert_eq!(runner, "test-runner");
            assert_eq!(api_version, "solti.io/v1");
            assert_eq!(kind, "Embedded");
        }
        Err(other) => panic!("expected UnsupportedWorkload, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok"),
    }
}

#[test]
fn workload_types_declares_subprocess() {
    let runner = SubprocessRunner::new("test").unwrap();
    let task = mk_subprocess_spec("s", "echo");
    assert_eq!(
        runner.workload_types(),
        vec![task.spec().workload().type_meta()]
    );
}

#[tokio::test]
async fn script_task_runs_and_streams_output() {
    use solti_model::OutputEvent;

    let (ctx, rx, _calls) = recording_output_context();

    let runner = SubprocessRunner::new("test-runner").unwrap();
    let spec = mk_script_spec("script-e2e", b"echo \"hello-$1\"", &["script"]);
    let task_ref = build_with_run_id(&runner, &spec, &ctx).await.unwrap();
    let cancel = TaskContext::detached();
    task_ref
        .spawn(cancel)
        .await
        .expect("script task must succeed");

    let found = rx.try_iter().any(|event| {
        let OutputEvent::Chunk(chunk) = event else {
            return false;
        };
        std::str::from_utf8(&chunk.line)
            .unwrap_or_default()
            .contains("hello-script")
    });
    assert!(
        found,
        "script output must reach the registry (anonymous transport created at run time, extra args preserved)"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires root with only CAP_SETUID and CAP_SETGID"]
async fn script_task_runs_after_exact_credential_change() {
    use crate::host::{HostProcessPolicy, LinuxCapability, ProcessCredentials, SecurityConfig};

    // SAFETY: `geteuid` has no preconditions.
    assert_eq!(unsafe { libc::geteuid() }, 0, "test requires root");
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
        .and_then(|value| u64::from_str_radix(value, 16).ok())
        .expect("CapEff is missing from /proc/self/status");
    let has = |capability: LinuxCapability| effective & (1_u64 << capability.to_cap_value()) != 0;
    assert!(has(LinuxCapability::SetUid), "CAP_SETUID is required");
    assert!(has(LinuxCapability::SetGid), "CAP_SETGID is required");
    assert!(!has(LinuxCapability::Chown), "CAP_CHOWN must be absent");
    assert!(!has(LinuxCapability::FOwner), "CAP_FOWNER must be absent");
    if let Ok(setgroups) = std::fs::read_to_string("/proc/self/setgroups") {
        assert_ne!(setgroups.trim(), "deny", "setgroups must be permitted");
    }

    let backend = SubprocessBackendConfig::new().with_host_process_policy(
        HostProcessPolicy::new().with_security(SecurityConfig {
            credentials: Some(ProcessCredentials::new(65_534, 65_534)),
            no_new_privs: true,
            ..Default::default()
        }),
    );
    let runner = SubprocessRunner::with_config("credential-test", backend).unwrap();
    let build = BuildContext::default();
    let cancel = TaskContext::detached();

    let script = mk_script_spec("script-credentials", b"exit 0", &[]);
    let script_ref = build_with_run_id(&runner, &script, &build).await.unwrap();
    script_ref
        .spawn(cancel)
        .await
        .expect("sealed script must remain readable after changing credentials");
}

#[tokio::test]
async fn script_task_can_be_spawned_repeatedly() {
    // Anonymous backing storage is materialized per attempt.
    // A retry must receive a fresh descriptor with the same body.
    let runner = SubprocessRunner::new("test-runner").unwrap();
    let spec = mk_script_spec("script-retry", b"exit 0", &[]);
    let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default())
        .await
        .unwrap();

    let ctx = TaskContext::detached();
    task_ref
        .spawn(ctx.clone())
        .await
        .expect("first attempt must succeed");
    task_ref
        .spawn(ctx)
        .await
        .expect("second attempt must succeed");
}

#[test]
fn resolve_mode_command() {
    let mode = solti_model::SubprocessMode::Command {
        command: "ls".into(),
        args: vec!["-la".into()],
    };
    let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
    assert_eq!(r.command, "ls");
    assert_eq!(r.args, vec!["-la"]);
    assert!(r.script_body.is_none(), "Command mode carries no script");
}

#[test]
fn resolve_mode_script_defers_transport_to_run_time() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let mode = solti_model::SubprocessMode::Script {
        interpreter: "bash".into(),
        body: BASE64.encode(b"echo hello"),
        args: vec!["extra".into()],
    };
    let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
    assert_eq!(r.command, "bash");
    assert_eq!(
        r.args,
        vec!["extra"],
        "resolve must not create backing storage: the descriptor path is prepended at spawn time"
    );

    let body = r.script_body.expect("Script mode must carry the body");
    assert_eq!(&*body, "echo hello");
}

#[test]
fn resolve_mode_script_uses_explicit_interpreter() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let mode = solti_model::SubprocessMode::Script {
        interpreter: "ruby".into(),
        body: BASE64.encode(b"puts 'hi'"),
        args: vec![],
    };
    let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap();
    assert_eq!(r.command, "ruby");
    assert!(r.args.is_empty());
    assert!(r.script_body.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_reaps_forked_grandchildren() {
    use std::process::Stdio;
    use tokio::process::Command as TokioCommand;

    let marker_dir = tempfile::TempDir::new().unwrap();
    let marker = marker_dir.path().join("pid");
    let marker_str = marker.to_string_lossy().to_string();

    let script = format!(
        r#"
            (sleep 60 & echo $! > "{marker}") &
            wait
            "#,
        marker = marker_str
    );

    let mut cmd = TokioCommand::new("bash");
    cmd.args(["-c", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.process_group(0);

    let host = crate::host::HostProcessPolicy::new()
        .prepare()
        .unwrap()
        .prepare_attempt(None)
        .unwrap()
        .apply_to_command(cmd.as_std_mut());
    let child = cmd.spawn().expect("bash must spawn");
    let finalizer = DropFinalizerDomain::start(8).unwrap();
    let mut process = ActiveProcessDomain::new(
        child,
        host,
        Arc::from("test"),
        finalizer.try_reserve().unwrap(),
    );
    let grandchild_pid = wait_for_recorded_pid(&marker).await;
    if let Some(pid) = grandchild_pid {
        assert_eq!(
            // SAFETY: signal zero performs an existence check and `pid` was
            // reported by the child process started by this test.
            unsafe { libc::kill(pid, 0) },
            0,
            "grandchild must be alive before cancel"
        );
    }

    process.terminate().unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), process.reap()).await;
    let grandchild_pid = grandchild_pid.expect("grandchild did not report its pid");
    assert_process_gone(grandchild_pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_the_future_kills_the_whole_process_group() {
    // Taskvisor enforces a per-attempt timeout via `tokio::time::timeout` and
    // requests force-abort through `JoinHandle::abort`. This Tokio-cooperative
    // task yields, then drops without polling the `cancel.cancelled()` branch.
    // The active process domain must still stop the process group on drop.
    let marker_dir = tempfile::TempDir::new().unwrap();
    let leader_marker = marker_dir.path().join("leader.pid");
    let descendant_marker = marker_dir.path().join("descendant.pid");
    let leader_marker = leader_marker.to_string_lossy().to_string();
    let descendant_marker = descendant_marker.to_string_lossy().to_string();

    // Record both identities before blocking on a long-lived descendant.
    let script = format!(
        r#"echo $$ > "{leader_marker}"; (sleep 60 & echo $! > "{descendant_marker}"); sleep 60"#
    );

    let runner = SubprocessRunner::new("test-runner").unwrap();
    let spec = mk_subprocess_spec_with_args("drop-slot", "bash", &["-c", &script]);
    let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default())
        .await
        .unwrap();

    let cancel = TaskContext::detached();
    let handle = tokio::spawn(async move { task_ref.spawn(cancel).await });
    let leader_pid = wait_for_recorded_pid(std::path::Path::new(&leader_marker)).await;
    let descendant_pid = wait_for_recorded_pid(std::path::Path::new(&descendant_marker)).await;

    handle.abort();
    let _ = handle.await;
    assert_process_gone(leader_pid.expect("leader did not report its pid")).await;
    assert_process_gone(descendant_pid.expect("descendant did not report its pid")).await;
    runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
    let status = runner.finalizer_status();
    assert_eq!(status.owned(), 0);
    assert!(status.healthy());
}

#[cfg(unix)]
struct StartObservedReader {
    stream: tokio::net::UnixStream,
    started: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(unix)]
impl tokio::io::AsyncRead for StartObservedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

#[cfg(unix)]
#[tokio::test]
async fn force_aborting_outer_attempt_aborts_blocked_readers_and_closes_pipe_endpoints() {
    use tokio::io::AsyncReadExt as _;

    let (stdout_reader, mut stdout_peer) = tokio::net::UnixStream::pair().unwrap();
    let (stderr_reader, mut stderr_peer) = tokio::net::UnixStream::pair().unwrap();
    let (stdout_started_tx, stdout_started_rx) = tokio::sync::oneshot::channel();
    let (stderr_started_tx, stderr_started_rx) = tokio::sync::oneshot::channel();
    let attempt = tokio::spawn(async move {
        let _output = start_output_tasks(
            ChildOutput::new(StartObservedReader {
                stream: stdout_reader,
                started: Some(stdout_started_tx),
            }),
            ChildOutput::new(StartObservedReader {
                stream: stderr_reader,
                started: Some(stderr_started_tx),
            }),
            Arc::from("force-drop-test"),
            LogConfig::default(),
            None,
        );
        std::future::pending::<()>().await;
    });

    stdout_started_rx
        .await
        .expect("stdout reader must poll its pipe");
    stderr_started_rx
        .await
        .expect("stderr reader must poll its pipe");
    let mut stdout_probe = [0_u8; 1];
    let mut stderr_probe = [0_u8; 1];
    let mut stdout_read = Box::pin(stdout_peer.read(&mut stdout_probe));
    let mut stderr_read = Box::pin(stderr_peer.read(&mut stderr_probe));
    assert_future_pending(stdout_read.as_mut());
    assert_future_pending(stderr_read.as_mut());
    drop(stdout_read);
    drop(stderr_read);

    // Taskvisor force-abort reaches this same Tokio abort boundary. Awaiting the
    // join error proves the outer future and its `OutputTasks` local were dropped.
    attempt.abort();
    assert!(attempt.await.unwrap_err().is_cancelled());

    assert_eq!(
        tokio::time::timeout(
            StdDuration::from_secs(1),
            stdout_peer.read(&mut stdout_probe)
        )
        .await
        .expect("stdout peer must observe endpoint release")
        .expect("stdout peer read must succeed"),
        0,
        "stdout peer EOF proves the reader endpoint was released",
    );
    assert_eq!(
        tokio::time::timeout(
            StdDuration::from_secs(1),
            stderr_peer.read(&mut stderr_probe)
        )
        .await
        .expect("stderr peer must observe endpoint release")
        .expect("stderr peer read must succeed"),
        0,
        "stderr peer EOF proves the reader endpoint was released",
    );
}

#[test]
fn resolve_mode_invalid_base64() {
    let mode = solti_model::SubprocessMode::Script {
        interpreter: "bash".into(),
        body: "not-valid!!!".into(),
        args: vec![],
    };
    let err =
        SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES).unwrap_err();
    assert!(matches!(err, RunnerError::InvalidSpec(_)));
}

#[tokio::test]
async fn subprocess_streams_stdout_into_output_publisher() {
    use solti_model::OutputEvent;

    let (ctx, rx, calls) = recording_output_context();

    let runner = SubprocessRunner::new("test-runner").unwrap();
    let spec = mk_subprocess_spec_with_args("echo-slot", "echo", &["hello-stream"]);
    let task_ref = build_with_run_id(&runner, &spec, &ctx).await.unwrap();
    let cancel = TaskContext::detached();
    task_ref.spawn(cancel).await.expect("echo must succeed");

    let chunk = rx
        .try_iter()
        .find_map(|event| match event {
            OutputEvent::Chunk(chunk)
                if std::str::from_utf8(&chunk.line)
                    .unwrap_or_default()
                    .contains("hello-stream") =>
            {
                Some(chunk)
            }
            _ => None,
        })
        .expect("expected to receive 'hello-stream' line");
    assert_eq!(chunk.attempt, 1);
    assert_eq!(chunk.generation, 1);
    assert_eq!(chunk.stream, solti_model::StreamKind::Stdout);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.as_slice(),
        &[(solti_model::TaskId::new("task-echo-slot").unwrap(), 1, 1)]
    );
}

#[tokio::test]
async fn subprocess_attempt_counter_increments_on_each_spawn() {
    use solti_model::OutputEvent;

    let (ctx, rx, _calls) = recording_output_context();
    let runner = SubprocessRunner::new("test-runner").unwrap();
    let spec = mk_subprocess_spec_with_args("attempts-slot", "echo", &["x"]);
    let task_ref = build_with_run_id(&runner, &spec, &ctx).await.unwrap();
    let ctx = TaskContext::detached();
    task_ref.spawn(ctx.clone()).await.unwrap();
    task_ref.spawn(ctx).await.unwrap();

    let attempts: std::collections::BTreeSet<_> = rx
        .try_iter()
        .filter_map(|event| match event {
            OutputEvent::Chunk(chunk) => Some(chunk.attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, std::collections::BTreeSet::from([1, 2]));
}

#[tokio::test]
async fn attempt_is_allocated_before_spawn_failure() {
    let (ctx, _rx, calls) = recording_output_context();
    let runner = SubprocessRunner::new("test-runner").unwrap();
    let task = mk_subprocess_spec("failed-spawn", "/definitely/not/a/command");
    let task_ref = build_with_run_id(&runner, &task, &ctx).await.unwrap();

    assert!(task_ref.spawn(TaskContext::detached()).await.is_err());
    assert!(task_ref.spawn(TaskContext::detached()).await.is_err());

    {
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            &[
                (solti_model::TaskId::new("task-failed-spawn").unwrap(), 1, 1,),
                (solti_model::TaskId::new("task-failed-spawn").unwrap(), 1, 2,),
            ]
        );
    }
    runner.shutdown(StdDuration::from_secs(2)).await.unwrap();
    assert_eq!(runner.finalizer_status().owned(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn daemonized_grandchild_cannot_hold_output_open() {
    let runner = SubprocessRunner::new("hang-runner").unwrap();
    let spec = mk_subprocess_spec_with_args("hang-slot", "sh", &["-c", "sleep 30 & exit 0"]);
    let task_ref = build_with_run_id(&runner, &spec, &BuildContext::default())
        .await
        .unwrap();

    let ctx = TaskContext::detached();
    tokio::time::timeout(std::time::Duration::from_secs(2), task_ref.spawn(ctx))
        .await
        .expect("output drain must be bounded")
        .expect("leader exited successfully");
}

#[cfg(unix)]
#[tokio::test]
async fn successful_task_kills_descendants_with_detached_output() {
    let marker_dir = tempfile::TempDir::new().unwrap();
    let marker = marker_dir.path().join("pid");
    let script = format!(
        "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > \"{}\"; exit 0",
        marker.display()
    );

    let runner = SubprocessRunner::new("descendant-runner").unwrap();
    let spec = mk_subprocess_spec_with_args("descendant-slot", "sh", &["-c", &script]);
    let task = build_with_run_id(&runner, &spec, &BuildContext::default())
        .await
        .unwrap();
    task.spawn(TaskContext::detached()).await.unwrap();

    let pid = wait_for_recorded_pid(&marker)
        .await
        .expect("descendant did not report its pid");
    assert_process_gone(pid).await;
}

#[test]
fn resolve_mode_script_accepts_large_body() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let payload: Vec<u8> = b"# "
        .iter()
        .copied()
        .chain(std::iter::repeat_n(b'x', 200 * 1024))
        .collect();
    let mode = solti_model::SubprocessMode::Script {
        interpreter: "bash".into(),
        body: BASE64.encode(&payload),
        args: vec![],
    };
    let r = SubprocessRunner::resolve_mode(&mode, solti_model::MAX_SCRIPT_BODY_BYTES)
        .expect("200 KiB script must resolve via descriptor transport");
    assert_eq!(r.command, "bash");
    assert!(r.args.is_empty());
    let body = r
        .script_body
        .expect("large Script must carry the decoded body");
    assert_eq!(body.len(), payload.len());
}

#[test]
fn resolve_mode_rejects_body_over_configured_limit() {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let mode = solti_model::SubprocessMode::Script {
        interpreter: "bash".into(),
        body: BASE64.encode("a".repeat(100).as_bytes()),
        args: vec![],
    };
    let err = SubprocessRunner::resolve_mode(&mode, 10)
        .expect_err("body over the configured limit must be rejected");
    assert!(
        matches!(err, RunnerError::InvalidSpec(_)),
        "expected InvalidSpec, got {err:?}"
    );
}
