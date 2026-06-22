//! Tripwire for the taskvisor `reason`-string coupling.
//!
//! solti-core's [`StateSubscriber`] maps taskvisor terminal/rejection events to
//! a [`TaskPhase`] **only** by the event's free-form `reason` string (taskvisor
//! exposes no typed discriminator). These tests drive a real taskvisor runtime
//! and assert that the strings in [`solti_core::reasons`] are still the ones
//! taskvisor emits — so a taskvisor rename fails CI here instead of silently
//! mis-classifying a run in production.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use solti_core::reasons;
use taskvisor::{
    AdmissionPolicy, BackoffPolicy, ControllerConfig, ControllerSpec, Event, EventKind,
    JitterPolicy, RestartPolicy, Subscribe, Supervisor, SupervisorConfig, TaskError, TaskFn,
    TaskRef, TaskSpec,
};
use tokio_util::sync::CancellationToken;

type Captured = Arc<Mutex<Vec<(EventKind, Option<String>)>>>;

/// Captures `(kind, reason)` for every event the supervisor publishes.
#[derive(Default)]
struct Capture {
    events: Captured,
}

impl Subscribe for Capture {
    fn on_event(&self, event: &Event) {
        self.events
            .lock()
            .unwrap()
            .push((event.kind, event.reason.as_ref().map(|s| s.to_string())));
    }

    fn name(&self) -> &'static str {
        "reason-capture"
    }
}

fn backoff() -> BackoffPolicy {
    BackoffPolicy {
        first: Duration::from_millis(1),
        max: Duration::from_millis(1),
        jitter: JitterPolicy::None,
        factor: 1.0,
    }
}

/// Polls the captured events until one carries `reason`, or fails after ~3s.
async fn wait_for_reason(events: &Captured, reason: &str) {
    for _ in 0..300 {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, r)| r.as_deref() == Some(reason))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "taskvisor never emitted the expected reason {reason:?}; captured = {:?}",
        events.lock().unwrap()
    );
}

#[tokio::test]
async fn taskvisor_emits_policy_exhausted_success() {
    let cap = Capture::default();
    let events = Arc::clone(&cap.events);
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![Arc::new(cap)])
        .build();
    let handle = sup.serve();

    let task: TaskRef = TaskFn::arc("ok-once", |_ctx: CancellationToken| async move {
        Ok::<(), TaskError>(())
    });
    let spec = TaskSpec::new(
        task,
        RestartPolicy::Never,
        backoff(),
        Some(Duration::from_secs(5)),
    );
    handle.add(spec).expect("add");

    wait_for_reason(&events, reasons::POLICY_EXHAUSTED_SUCCESS).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn taskvisor_emits_task_returned_canceled() {
    let cap = Capture::default();
    let events = Arc::clone(&cap.events);
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![Arc::new(cap)])
        .build();
    let handle = sup.serve();

    // A body returning TaskError::Canceled *without* a runtime-token cancel.
    let task: TaskRef = TaskFn::arc("self-cancel", |_ctx: CancellationToken| async move {
        Err::<(), TaskError>(TaskError::Canceled)
    });
    let spec = TaskSpec::new(
        task,
        RestartPolicy::Never,
        backoff(),
        Some(Duration::from_secs(5)),
    );
    handle.add(spec).expect("add");

    wait_for_reason(&events, reasons::TASK_RETURNED_CANCELED).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn taskvisor_emits_superseded_by_replace() {
    let cap = Capture::default();
    let events = Arc::clone(&cap.events);
    let sup = Supervisor::builder(SupervisorConfig::default())
        .with_subscribers(vec![Arc::new(cap)])
        .with_controller(ControllerConfig::default())
        .build();
    let handle = sup.serve();

    // The head task ignores cancellation for a while, so the slot stays in
    // `Terminating` after the first Replace — long enough for a queued head to
    // exist and then be displaced by a third submission.
    let stubborn: TaskRef = TaskFn::arc("head", |_ctx: CancellationToken| async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok::<(), TaskError>(())
    });
    let cooperative = |name: &'static str| -> TaskRef {
        TaskFn::arc(name, |ctx: CancellationToken| async move {
            ctx.cancelled().await;
            Ok::<(), TaskError>(())
        })
    };
    let mk_spec = |task: TaskRef| {
        ControllerSpec::new(
            AdmissionPolicy::Replace,
            TaskSpec::new(
                task,
                RestartPolicy::Never,
                backoff(),
                Some(Duration::from_secs(30)),
            )
            .with_slot("replace-slot"),
        )
    };

    // A: occupies the slot and resists cancellation (keeps the slot Terminating).
    let (_a, _aw) = handle
        .submit_and_watch(mk_spec(stubborn))
        .await
        .expect("submit A");
    tokio::time::sleep(Duration::from_millis(50)).await;
    // B: Replace -> A goes Terminating, B becomes the queued head.
    let (_b, _bw) = handle
        .submit_and_watch(mk_spec(cooperative("queued")))
        .await
        .expect("submit B");
    // C: Replace -> displaces the queued head B with `superseded_by_replace`.
    let (_c, _cw) = handle
        .submit_and_watch(mk_spec(cooperative("winner")))
        .await
        .expect("submit C");

    wait_for_reason(&events, reasons::SUPERSEDED_BY_REPLACE).await;
    let _ = handle.shutdown().await;
}
