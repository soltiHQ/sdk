use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use taskvisor::{
    AdmissionPolicy, BackoffPolicy, ControllerConfig, ControllerSpec, Event, EventKind,
    JitterPolicy, RejectionKind, RestartPolicy, Subscribe, Supervisor, SupervisorConfig,
    TaskContext, TaskError, TaskFn, TaskOutcome, TaskOutcomeKind, TaskRef, TaskSpec, TaskWaiter,
};
use tokio::sync::Notify;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Capture {
    events: Mutex<Vec<Event>>,
    changed: Notify,
}

impl Capture {
    async fn wait_for(&self, predicate: impl Fn(&Event) -> bool) -> Event {
        let found = tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                let changed = self.changed.notified();
                if let Some(event) = self
                    .events
                    .lock()
                    .expect("capture lock")
                    .iter()
                    .find(|event| predicate(event))
                    .cloned()
                {
                    return event;
                }
                changed.await;
            }
        })
        .await;

        match found {
            Ok(event) => event,
            Err(_) => panic!(
                "expected Taskvisor event was not observed; captured = {:?}",
                self.events.lock().expect("capture lock")
            ),
        }
    }
}

impl Subscribe for Capture {
    fn on_event(&self, event: &Event) {
        self.events
            .lock()
            .expect("capture lock")
            .push(event.clone());
        self.changed.notify_one();
    }

    fn name(&self) -> &'static str {
        "sdk-taskvisor-contract"
    }
}

fn backoff() -> BackoffPolicy {
    BackoffPolicy::new(
        Duration::from_millis(1),
        Duration::from_millis(1),
        1.0,
        JitterPolicy::None,
    )
    .expect("valid backoff")
}

fn task_spec(task: TaskRef) -> TaskSpec {
    TaskSpec::new(
        task,
        RestartPolicy::Never,
        backoff(),
        Some(Duration::from_secs(30)),
    )
}

fn controller_spec(policy: AdmissionPolicy, slot: &'static str, task: TaskRef) -> ControllerSpec {
    ControllerSpec::new(policy, task_spec(task)).with_slot(slot)
}

async fn wait_outcome(waiter: TaskWaiter) -> TaskOutcome {
    tokio::time::timeout(TEST_TIMEOUT, waiter.wait())
        .await
        .expect("Taskvisor waiter timed out")
        .expect("Taskvisor waiter closed without an outcome")
}

#[tokio::test]
async fn completed_waiter_and_event_share_the_typed_outcome() {
    let capture = Arc::new(Capture::default());
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![capture.clone()])
        .build();
    let handle = supervisor.serve();

    let task: TaskRef = TaskFn::arc("contract-completed", |_ctx: TaskContext| async move {
        Ok::<(), TaskError>(())
    });
    let (id, waiter) = handle
        .add_and_watch(task_spec(task))
        .await
        .expect("add watched task");

    assert!(matches!(wait_outcome(waiter).await, TaskOutcome::Completed));
    let event = capture
        .wait_for(|event| event.id == Some(id) && event.kind == EventKind::TaskFinished)
        .await;
    assert_eq!(event.outcome_kind, Some(TaskOutcomeKind::Completed));

    tokio::time::timeout(TEST_TIMEOUT, handle.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
}

#[tokio::test]
async fn canceled_attempt_and_task_have_typed_events() {
    let capture = Arc::new(Capture::default());
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![capture.clone()])
        .build();
    let handle = supervisor.serve();

    let task: TaskRef = TaskFn::arc("contract-canceled", |_ctx: TaskContext| async move {
        Err::<(), TaskError>(TaskError::Canceled)
    });
    let (id, waiter) = handle
        .add_and_watch(task_spec(task))
        .await
        .expect("add watched task");

    assert!(matches!(wait_outcome(waiter).await, TaskOutcome::Canceled));
    capture
        .wait_for(|event| event.id == Some(id) && event.kind == EventKind::AttemptCanceled)
        .await;
    let finished = capture
        .wait_for(|event| event.id == Some(id) && event.kind == EventKind::TaskFinished)
        .await;
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Canceled));

    tokio::time::timeout(TEST_TIMEOUT, handle.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
}

#[tokio::test]
async fn configured_timeout_has_one_typed_terminal_attempt_event() {
    let capture = Arc::new(Capture::default());
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![capture.clone()])
        .build();
    let handle = supervisor.serve();

    let task: TaskRef = TaskFn::arc("contract-timeout", |_ctx: TaskContext| async move {
        std::future::pending::<()>().await;
        Ok::<(), TaskError>(())
    });
    let spec = TaskSpec::new(
        task,
        RestartPolicy::Never,
        backoff(),
        Some(Duration::from_millis(10)),
    );
    let (id, waiter) = handle.add_and_watch(spec).await.expect("add watched task");

    assert!(matches!(
        wait_outcome(waiter).await,
        TaskOutcome::Failed { .. }
    ));
    let timed_out = capture
        .wait_for(|event| event.id == Some(id) && event.kind == EventKind::AttemptTimedOut)
        .await;
    assert_eq!(timed_out.attempt, Some(1));
    assert_eq!(timed_out.timeout_ms, Some(10));

    let finished = capture
        .wait_for(|event| event.id == Some(id) && event.kind == EventKind::TaskFinished)
        .await;
    assert_eq!(finished.outcome_kind, Some(TaskOutcomeKind::Failed));
    assert!(
        !capture
            .events
            .lock()
            .expect("capture lock")
            .iter()
            .any(|event| event.id == Some(id) && event.kind == EventKind::AttemptFailed),
        "AttemptTimedOut is the timeout attempt's only terminal event"
    );

    tokio::time::timeout(TEST_TIMEOUT, handle.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
}

#[tokio::test]
async fn drop_if_running_is_classified_by_rejection_kind() {
    let capture = Arc::new(Capture::default());
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![capture.clone()])
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve();

    let started = Arc::new(Notify::new());
    let task_started = Arc::clone(&started);
    let busy: TaskRef = TaskFn::arc("contract-busy", move |ctx: TaskContext| {
        let task_started = Arc::clone(&task_started);
        async move {
            task_started.notify_one();
            ctx.cancelled().await;
            Ok::<(), TaskError>(())
        }
    });
    let dropped: TaskRef = TaskFn::arc("contract-dropped", |_ctx: TaskContext| async move {
        Ok::<(), TaskError>(())
    });

    let (_busy_id, _busy_waiter) = handle
        .submit_and_watch(controller_spec(
            AdmissionPolicy::DropIfRunning,
            "contract-drop-slot",
            busy,
        ))
        .await
        .expect("submit running task");
    tokio::time::timeout(TEST_TIMEOUT, started.notified())
        .await
        .expect("running task did not start");

    let (dropped_id, dropped_waiter) = handle
        .submit_and_watch(controller_spec(
            AdmissionPolicy::DropIfRunning,
            "contract-drop-slot",
            dropped,
        ))
        .await
        .expect("submit conflicting task");

    match wait_outcome(dropped_waiter).await {
        TaskOutcome::Rejected {
            kind: RejectionKind::SlotBusy,
            ..
        } => {}
        other => panic!("expected SlotBusy rejection, got {other:?}"),
    }
    let event = capture
        .wait_for(|event| {
            event.id == Some(dropped_id) && event.kind == EventKind::ControllerRejected
        })
        .await;
    assert_eq!(event.outcome_kind, Some(TaskOutcomeKind::Rejected));
    assert_eq!(event.rejection_kind, Some(RejectionKind::SlotBusy));

    tokio::time::timeout(TEST_TIMEOUT, handle.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
}

#[tokio::test]
async fn replace_supersedes_the_queued_submission_by_typed_kind() {
    let capture = Arc::new(Capture::default());
    let supervisor = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![capture.clone()])
        .with_controller(ControllerConfig::default())
        .build();
    let handle = supervisor.serve();

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task_started = Arc::clone(&started);
    let task_release = Arc::clone(&release);
    let head: TaskRef = TaskFn::arc("contract-head", move |_ctx: TaskContext| {
        let task_started = Arc::clone(&task_started);
        let task_release = Arc::clone(&task_release);
        async move {
            task_started.notify_one();
            task_release.notified().await;
            Ok::<(), TaskError>(())
        }
    });
    let queued: TaskRef = TaskFn::arc("contract-queued", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok::<(), TaskError>(())
    });
    let replacement: TaskRef = TaskFn::arc("contract-replacement", |ctx: TaskContext| async move {
        ctx.cancelled().await;
        Ok::<(), TaskError>(())
    });

    let (_head_id, _head_waiter) = handle
        .submit_and_watch(controller_spec(
            AdmissionPolicy::Replace,
            "contract-replace-slot",
            head,
        ))
        .await
        .expect("submit running head");
    tokio::time::timeout(TEST_TIMEOUT, started.notified())
        .await
        .expect("running head did not start");

    let (queued_id, queued_waiter) = handle
        .submit_and_watch(controller_spec(
            AdmissionPolicy::Replace,
            "contract-replace-slot",
            queued,
        ))
        .await
        .expect("queue first replacement");
    let (_replacement_id, _replacement_waiter) = handle
        .submit_and_watch(controller_spec(
            AdmissionPolicy::Replace,
            "contract-replace-slot",
            replacement,
        ))
        .await
        .expect("queue newer replacement");

    match wait_outcome(queued_waiter).await {
        TaskOutcome::Rejected {
            kind: RejectionKind::SupersededByReplace,
            ..
        } => {}
        other => panic!("expected SupersededByReplace rejection, got {other:?}"),
    }
    let event = capture
        .wait_for(|event| {
            event.id == Some(queued_id) && event.kind == EventKind::ControllerRejected
        })
        .await;
    assert_eq!(event.outcome_kind, Some(TaskOutcomeKind::Rejected));
    assert_eq!(
        event.rejection_kind,
        Some(RejectionKind::SupersededByReplace)
    );

    release.notify_one();
    tokio::time::timeout(TEST_TIMEOUT, handle.shutdown())
        .await
        .expect("shutdown timed out")
        .expect("shutdown failed");
}
