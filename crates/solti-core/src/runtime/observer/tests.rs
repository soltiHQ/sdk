use super::*;
use taskvisor::TaskOutcomeKind;

use crate::output::{OutputConfig, OutputHub};
use crate::{PersistenceConfig, StateConfig, TaskStateEvent, TaskStateSink};
use solti_model::{
    ConditionStatus, EmbeddedSpec, OutputEvent, TaskManifest, TaskSpec, TaskWorkload,
};
use std::sync::atomic::{AtomicBool, Ordering};
use taskvisor::Event;

struct IgnoringStateSink;

impl TaskStateSink for IgnoringStateSink {
    fn on_event(&self, _event: &TaskStateEvent) {}
}

struct ArmableBlockingStateSink {
    block_next: AtomicBool,
    entered: std::sync::mpsc::SyncSender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl TaskStateSink for ArmableBlockingStateSink {
    fn on_event(&self, _event: &TaskStateEvent) {
        if self.block_next.swap(false, Ordering::AcqRel) {
            self.entered
                .send(())
                .expect("the test must observe the blocked persistence callback");
            self.release
                .lock()
                .recv()
                .expect("ordinary Tokio work must release the persistence callback");
        }
    }
}

async fn wait_for_condition(message: &'static str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect(message);
}

fn test_spec() -> TaskSpec {
    TaskSpec::builder(
        "slot",
        TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
        5_000_u64,
    )
    .build()
    .expect("valid spec")
}

fn changed_test_spec() -> TaskSpec {
    TaskSpec::builder(
        "slot",
        TaskWorkload::Embedded(EmbeddedSpec::new("test-v2").unwrap()),
        6_000_u64,
    )
    .build()
    .expect("valid spec")
}

fn add_test_task(state: &TaskState, task_name: &str) -> TaskId {
    let id = TaskId::new(task_name).unwrap();
    state.add_task(TaskManifest::new(id.clone(), test_spec()).expect("valid manifest"));
    id
}

fn bind_test_task(state: &TaskState, id: &TaskId, tv: taskvisor::TaskId) -> RuntimeBinding {
    let resource = ResourceGeneration::from_task(&state.get(id).expect("task must exist"));
    assert!(state.bind_tv(resource.clone(), tv));
    RuntimeBinding { resource, tv }
}

trait TestTaskStateExt {
    fn tv_for(&self, id: &TaskId) -> Option<taskvisor::TaskId>;
}

impl TestTaskStateExt for TaskState {
    fn tv_for(&self, id: &TaskId) -> Option<taskvisor::TaskId> {
        self.binding_for(id).map(|binding| binding.tv)
    }
}

fn setup(task_name: &str) -> (RuntimeObserver, TaskState, TaskId) {
    let state = TaskState::new();
    let id = add_test_task(&state, task_name);
    let binding = bind_test_task(&state, &id, taskvisor::TaskId::for_tests());
    assert!(state.transition_attempt_starting(&binding, 1));
    let sub = RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::new(OutputHub::new(OutputConfig::default())),
    );
    (sub, state, id)
}

fn bound_event(state: &TaskState, id: &TaskId, kind: EventKind) -> Event {
    Event::new(kind).with_id(state.tv_for(id).expect("task must be bound"))
}

#[tokio::test]
async fn prepared_binding_routes_the_first_event_without_replay() {
    let state = TaskState::new();
    let id = add_test_task(&state, "prepared-start");
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));
    let tv = taskvisor::TaskId::for_tests();
    let resource = ResourceGeneration::from_task(&state.get(&id).expect("task must exist"));

    let _provisional = sub
        .bind(resource, tv, true)
        .await
        .expect("current generation must bind");
    sub.on_event(
        &Event::new(EventKind::AttemptStarting)
            .with_id(tv)
            .with_attempt(1),
    );

    assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
    assert_eq!(state.list_runs(&id).len(), 1);
    assert_eq!(state.list_runs(&id)[0].attempt(), 1);
    assert!(registry.subscribe_raw(&id).is_some());
}

#[tokio::test]
async fn provisional_drop_notifies_an_existing_completion_barrier_waiter() {
    let state = TaskState::new();
    let id = add_test_task(&state, "provisional-drop-barrier");
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    let sub = Arc::new(RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::clone(&registry),
    ));
    let tv = taskvisor::TaskId::for_tests();
    let resource = ResourceGeneration::from_task(&state.get(&id).expect("task must exist"));
    let provisional = sub
        .bind(resource, tv, true)
        .await
        .expect("current generation must bind");

    let settle_sub = Arc::clone(&sub);
    let settlement = tokio::spawn(async move {
        settle_sub.settle_after_confirmed_cleanup(tv).await;
    });
    wait_for_condition("completion barrier waiter was not registered", || {
        sub.completion_barriers
            .lock()
            .notifications
            .contains_key(&tv.get())
    })
    .await;

    drop(provisional);
    tokio::time::timeout(Duration::from_secs(1), settlement)
        .await
        .expect("provisional Drop did not notify the completion barrier")
        .expect("completion barrier waiter panicked");
    assert!(state.binding_for(&id).is_none());
    assert!(registry.subscribe_raw(&id).is_none());
    let barriers = sub.completion_barriers.lock();
    assert!(!barriers.pending.contains_key(&tv.get()));
    assert!(!barriers.removed.contains(&tv.get()));
    assert!(!barriers.notifications.contains_key(&tv.get()));
    assert!(state.list_runs(&id).is_empty());
}

#[tokio::test]
async fn taskvisor_callback_after_state_admission_close_does_not_mutate_without_sink() {
    let state = TaskState::new();
    let id = add_test_task(&state, "late-closed-callback");
    let tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, tv);
    let observer = RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::new(OutputHub::new(OutputConfig::default())),
    );
    state.shutdown_persistence().await;
    let before = state.get(&id).unwrap();

    observer.on_event(
        &Event::new(EventKind::AttemptStarting)
            .with_id(tv)
            .with_attempt(1),
    );

    assert_eq!(state.get(&id).as_ref(), Some(&before));
    assert!(state.list_runs(&id).is_empty());
    assert_eq!(state.tv_for(&id), Some(tv));
}

#[test]
fn terminal_event_uses_authoritative_attempt_when_start_was_dropped() {
    let state = TaskState::new();
    let id = add_test_task(&state, "dropped-start");
    let tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let mut output = registry.subscribe_raw(&id).expect("output channel");
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    sub.on_event(
        &Event::new(EventKind::AttemptFailed)
            .with_id(tv)
            .with_attempt(2)
            .with_reason("start event was dropped")
            .with_exit_code(7),
    );

    let task = state.get(&id).expect("task exists");
    assert_eq!(task.status().attempt(), 2);
    assert_eq!(task.status().phase(), TaskPhase::Failed);
    let runs = state.list_runs(&id);
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].generation(), runs[0].attempt()), (1, 2));
    assert_eq!(runs[0].phase(), TaskPhase::Failed);
    assert!(matches!(
        output.try_recv(),
        Ok(OutputEvent::RunFinished {
            generation: 1,
            attempt: 2,
            exit_code: Some(7),
            ..
        })
    ));
}

#[test]
fn attempt_event_without_attempt_is_ignored_without_attempt_zero() {
    let state = TaskState::new();
    let id = add_test_task(&state, "missing-attempt");
    let tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let mut output = registry.subscribe_raw(&id).expect("output channel");
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    sub.on_event(
        &Event::new(EventKind::AttemptFailed)
            .with_id(tv)
            .with_reason("missing authoritative attempt"),
    );

    let task = state.get(&id).expect("task exists");
    assert_eq!(task.status().phase(), TaskPhase::Pending);
    assert_eq!(task.status().attempt(), 0);
    assert!(state.list_runs(&id).is_empty());
    assert!(output.try_recv().is_err());
}

#[test]
fn old_generation_attempt_closes_only_its_run() {
    let state = TaskState::new();
    let id = add_test_task(&state, "old-generation");
    let tv = taskvisor::TaskId::for_tests();
    let old_binding = bind_test_task(&state, &id, tv);
    assert!(state.transition_attempt_starting(&old_binding, 1));
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let mut output = registry.subscribe_raw(&id).expect("output channel");
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    let desired = TaskManifest::new(id.clone(), changed_test_spec()).expect("valid desired task");
    let commit = state.apply_desired(&desired).expect("apply must succeed");
    assert_eq!(commit.task.metadata().generation(), 2);
    assert_eq!(commit.task.status().phase(), TaskPhase::Pending);

    sub.on_event(
        &Event::new(EventKind::AttemptFailed)
            .with_id(tv)
            .with_attempt(1)
            .with_reason("old generation stopped")
            .with_exit_code(9),
    );

    let current = state.get(&id).expect("current task exists");
    assert_eq!(current.metadata().generation(), 2);
    assert_eq!(current.status().phase(), TaskPhase::Pending);
    assert_eq!(current.status().attempt(), 0);
    let runs = state.list_runs(&id);
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].generation(), runs[0].attempt()), (1, 1));
    assert_eq!(runs[0].phase(), TaskPhase::Failed);
    assert!(matches!(
        output.try_recv(),
        Ok(OutputEvent::RunFinished {
            generation: 1,
            attempt: 1,
            exit_code: Some(9),
            ..
        })
    ));
}

#[test]
fn stale_incarnation_event_cannot_touch_recreated_resource() {
    let state = TaskState::new();
    let id = add_test_task(&state, "recreated");
    let old_task = state.get(&id).expect("old task exists");
    let old_tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, old_tv);
    assert!(state.delete_task(&id));

    let recreated_id = add_test_task(&state, "recreated");
    let recreated = state.get(&recreated_id).expect("new task exists");
    assert_ne!(old_task.uid(), recreated.uid());
    let new_tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &recreated_id, new_tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(recreated_id.clone());
    let mut output = registry
        .subscribe_raw(&recreated_id)
        .expect("output channel");
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    sub.on_event(
        &Event::new(EventKind::AttemptFailed)
            .with_id(old_tv)
            .with_attempt(1)
            .with_reason("late old incarnation"),
    );

    let current = state.get(&recreated_id).expect("new task remains");
    assert_eq!(current.uid(), recreated.uid());
    assert_eq!(current.status().phase(), TaskPhase::Pending);
    assert!(state.list_runs(&recreated_id).is_empty());
    assert_eq!(state.tv_for(&recreated_id), Some(new_tv));
    assert!(output.try_recv().is_err());
}

#[tokio::test]
async fn failed_intake_cleanup_is_fenced_by_exact_binding() {
    let state = TaskState::new();
    let id = add_test_task(&state, "intake-failure");
    let old_tv = taskvisor::TaskId::for_tests();
    let stale = bind_test_task(&state, &id, old_tv);
    let current_tv = taskvisor::TaskId::for_tests();
    let current = bind_test_task(&state, &id, current_tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel_if_absent(id.clone(), current.resource.uid.clone());
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    assert!(
        !sub.fail_bound_reconciliation(
            &stale,
            "RuntimeSubmissionFailed",
            "stale intake failure".into(),
        )
        .await
    );
    assert_eq!(state.tv_for(&id), Some(current_tv));
    assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Pending);
    assert!(registry.subscribe_raw(&id).is_some());

    assert!(
        sub.fail_bound_reconciliation(
            &current,
            "RuntimeSubmissionFailed",
            "controller intake failed".into(),
        )
        .await
    );
    assert!(state.tv_for(&id).is_none());
    let failed = state.get(&id).expect("desired resource is retained");
    assert_eq!(failed.status().phase(), TaskPhase::Pending);
    assert_eq!(failed.status().observed_generation(), 1);
    assert_eq!(failed.status().attempt(), 0);
    assert!(failed.status().error().is_none());
    assert_eq!(
        failed.status().reconciled().status(),
        ConditionStatus::False
    );
    assert_eq!(
        failed.status().reconciled().reason(),
        "RuntimeSubmissionFailed"
    );
    assert_eq!(
        failed.status().reconciled().message(),
        "controller intake failed"
    );
    assert!(registry.subscribe_raw(&id).is_none());
}

#[tokio::test]
async fn task_removed_barrier_preserves_queued_attempt_events_before_cleanup() {
    let state = TaskState::new();
    let id = add_test_task(&state, "fast-attempt");
    let tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let mut output = registry.subscribe_raw(&id).expect("output channel");
    let sub = Arc::new(RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::clone(&registry),
    ));

    let completion = {
        let sub = Arc::clone(&sub);
        tokio::spawn(async move {
            sub.finalize_from_outcome(tv.get(), &taskvisor::TaskOutcome::Completed)
                .await;
        })
    };
    tokio::task::yield_now().await;

    sub.on_event(
        &Event::new(EventKind::AttemptStarting)
            .with_id(tv)
            .with_attempt(1),
    );
    sub.on_event(
        &Event::new(EventKind::AttemptSucceeded)
            .with_id(tv)
            .with_attempt(1),
    );
    sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));
    completion.await.expect("completion task");

    let task = state.get(&id).expect("retained terminal task");
    assert_eq!(task.status().phase(), TaskPhase::Succeeded);
    assert_eq!(state.list_runs(&id).len(), 1);
    assert_eq!(state.list_runs(&id)[0].phase(), TaskPhase::Succeeded);
    assert!(state.tv_for(&id).is_none());
    assert!(registry.subscribe_raw(&id).is_none());
    assert!(matches!(
        output.try_recv(),
        Ok(OutputEvent::RunStarted { attempt: 1, .. })
    ));
    assert!(matches!(
        output.try_recv(),
        Ok(OutputEvent::RunFinished { attempt: 1, .. })
    ));
}

#[tokio::test]
async fn confirmed_shutdown_cleans_pending_binding_when_state_admission_is_closed() {
    let state = TaskState::try_with_config_and_sink(
        StateConfig::default(),
        Some(Arc::new(IgnoringStateSink)),
    )
    .unwrap();
    let id = add_test_task(&state, "shutdown-pending-closed-admission");
    let before = state.get(&id).unwrap();
    let tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel_if_absent(id.clone(), before.uid().clone());
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    sub.finalize_unavailable(tv.get(), "outcome channel closed".into())
        .await;
    assert_eq!(state.tv_for(&id), Some(tv));
    assert!(registry.subscribe_raw(&id).is_some());

    wait_for_condition("initial state event did not drain", || {
        state.persistence_status().is_some_and(|status| {
            status.healthy() && status.queued() == 0 && status.delivered() == 1
        })
    })
    .await;
    state.inject_persistence_worker_panic();
    state.add_task(TaskManifest::new("shutdown-pending-panic-trigger", test_spec()).unwrap());
    wait_for_condition("state persistence admission did not close", || {
        state
            .persistence_status()
            .is_some_and(|status| !status.accepting() && !status.healthy())
    })
    .await;

    sub.finalize_pending_after_confirmed_shutdown().await;
    assert!(state.tv_for(&id).is_none());
    assert!(registry.subscribe_raw(&id).is_none());
    assert_eq!(
        state.get(&id).as_ref(),
        Some(&before),
        "cleanup-only shutdown fallback must not synthesize Task status"
    );
    assert!(state.list_runs(&id).is_empty());
}

#[test]
fn late_events_after_completion_are_ignored() {
    let (sub, state, id) = setup("late-after-complete");
    let tv = state.tv_for(&id).expect("bound task");

    sub.finalize_outcome_immediately_for_test(tv.get(), &taskvisor::TaskOutcome::Completed);
    sub.on_event(
        &Event::new(EventKind::AttemptStarting)
            .with_id(tv)
            .with_attempt(2),
    );
    sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));

    assert!(state.tv_for(&id).is_none());
    assert_eq!(
        state.get(&id).unwrap().status().phase(),
        TaskPhase::Succeeded
    );
}

#[tokio::test]
async fn late_outcome_after_explicit_delete_skips_the_barrier_wait() {
    let (sub, state, id) = setup("deleted-before-outcome");
    let tv = state.tv_for(&id).expect("bound task");

    assert!(sub.delete_after_cleanup(&id, Some(tv)).await.unwrap());
    tokio::time::timeout(
        Duration::from_millis(100),
        sub.finalize_from_outcome(tv.get(), &taskvisor::TaskOutcome::Completed),
    )
    .await
    .expect("a completed identity must not create a new barrier");

    assert!(state.get(&id).is_none());
}

#[tokio::test]
async fn idempotent_delete_does_not_evict_an_unknown_external_channel() {
    let state = TaskState::new();
    let id = TaskId::new("not-yet-submitted").unwrap();
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let sub = RuntimeObserver::with_output_hub(state, Arc::clone(&registry));

    assert!(!sub.delete_after_cleanup(&id, None).await.unwrap());
    assert!(registry.subscribe_raw(&id).is_some());
}

#[tokio::test]
async fn waiter_error_releases_binding_only_after_task_removed_barrier() {
    let (sub, state, id) = setup("missing-outcome");
    let tv = state.tv_for(&id).expect("bound task");

    sub.finalize_unavailable(tv.get(), "task outcome unavailable: shutting down".into())
        .await;
    assert!(
        state.tv_for(&id).is_some(),
        "channel closure alone must fail closed while task cleanup is unproven"
    );

    sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));

    assert!(state.tv_for(&id).is_none());
    let task = state.get(&id).expect("retained failed task");
    assert_eq!(task.status().phase(), TaskPhase::Failed);
    assert!(
        task.status()
            .error()
            .is_some_and(|error| error.contains("outcome unavailable"))
    );
}

#[tokio::test]
async fn pending_finalization_does_not_retain_persistence_capacity_for_its_barrier() {
    let state = TaskState::try_with_config_sink_and_persistence(
        StateConfig::new(),
        Some(Arc::new(IgnoringStateSink)),
        PersistenceConfig::new()
            .try_with_state_queue_capacity(2)
            .unwrap(),
    )
    .unwrap();
    let id = add_test_task(&state, "pending-finalization-capacity");
    let tv = taskvisor::TaskId::for_tests();
    let binding = bind_test_task(&state, &id, tv);
    assert!(state.transition_attempt_starting(&binding, 1));
    let sub = RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::new(OutputHub::new(OutputConfig::default())),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.persistence_status().unwrap().queued() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("setup persistence events must drain");

    sub.finalize_unavailable(tv.get(), "outcome unavailable".into())
        .await;
    assert_eq!(state.persistence_status().unwrap().queued(), 0);
    assert!(state.tv_for(&id).is_some());

    sub.on_event(&Event::new(EventKind::TaskRemoved).with_id(tv));
    assert!(state.tv_for(&id).is_none());
    state.shutdown_persistence().await;
}

#[tokio::test(flavor = "current_thread")]
async fn overflow_finalizes_safe_pending_identities_one_bounded_batch_at_a_time() {
    let state = TaskState::try_with_config_sink_and_persistence(
        StateConfig::new(),
        Some(Arc::new(IgnoringStateSink)),
        PersistenceConfig::new()
            .try_with_state_queue_capacity(2)
            .unwrap(),
    )
    .unwrap();
    let first_id = add_test_task(&state, "overflow-first");
    let first_tv = taskvisor::TaskId::for_tests();
    let first_binding = bind_test_task(&state, &first_id, first_tv);
    assert!(state.transition_attempt_starting(&first_binding, 1));
    let second_id = add_test_task(&state, "overflow-second");
    let second_tv = taskvisor::TaskId::for_tests();
    let second_binding = bind_test_task(&state, &second_id, second_tv);
    assert!(state.transition_attempt_starting(&second_binding, 1));
    wait_for_condition("setup persistence events must drain", || {
        state.persistence_status().unwrap().queued() == 0
    })
    .await;

    let observer = Arc::new(RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::new(OutputHub::new(OutputConfig::default())),
    ));
    {
        let _lifecycle = observer.lifecycle_gate.lock().await;
        for tv in [first_tv, second_tv] {
            let (ready, notification) = observer.register_finalization_locked(
                tv.get(),
                Finalization {
                    phase: TaskPhase::Failed,
                    error: Some("subscriber overflow".into()),
                    exit_code: None,
                    force: true,
                    safe_without_barrier: true,
                },
                true,
            );
            assert!(ready.is_none());
            assert!(notification.is_some());
        }
    }

    let event = Event::new(EventKind::SubscriberOverflow)
        .with_task(observer.name())
        .with_dropped(1);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let callback_observer = Arc::clone(&observer);
    let callback = std::thread::spawn(move || {
        callback_observer.on_event(&event);
        done_tx
            .send(())
            .expect("the test must observe overflow completion");
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("overflow must finalize more identities than one batch can reserve");
    callback.join().expect("the callback thread must finish");
    wait_for_condition("overflow persistence events must drain", || {
        state.persistence_status().unwrap().queued() == 0
    })
    .await;

    for id in [&first_id, &second_id] {
        assert!(state.tv_for(id).is_none());
        assert_eq!(state.get(id).unwrap().status().phase(), TaskPhase::Failed);
        assert_eq!(state.list_runs(id)[0].phase(), TaskPhase::Failed);
    }
    state.shutdown_persistence().await;
}

#[tokio::test(flavor = "current_thread")]
async fn persistence_admission_precedes_lifecycle_gate_for_async_and_callback_writes() {
    let (sink_entered_tx, sink_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (sink_release_tx, sink_release_rx) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(ArmableBlockingStateSink {
        block_next: AtomicBool::new(false),
        entered: sink_entered_tx,
        release: Mutex::new(sink_release_rx),
    });
    let state = TaskState::try_with_config_sink_and_persistence(
        StateConfig::new(),
        Some(sink.clone()),
        PersistenceConfig::new()
            .try_with_state_queue_capacity(2)
            .unwrap(),
    )
    .unwrap();
    let callback_id = add_test_task(&state, "callback-terminal");
    let callback_tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &callback_id, callback_tv);
    let finalizer_id = add_test_task(&state, "async-finalizer");
    let finalizer_tv = taskvisor::TaskId::for_tests();
    let finalizer_binding = bind_test_task(&state, &finalizer_id, finalizer_tv);
    assert!(state.transition_attempt_starting(&finalizer_binding, 1));
    wait_for_condition("setup persistence events must drain", || {
        state.persistence_status().unwrap().queued() == 0
    })
    .await;

    let observer = Arc::new(RuntimeObserver::with_output_hub(
        state.clone(),
        Arc::new(OutputHub::new(OutputConfig::default())),
    ));
    observer
        .finalize_unavailable(callback_tv.get(), "outcome unavailable".into())
        .await;
    assert_eq!(state.tv_for(&callback_id), Some(callback_tv));
    sink.block_next.store(true, Ordering::Release);
    add_test_task(&state, "active-persistence-callback");
    sink_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("one persistence callback must stay active");
    assert_eq!(state.persistence_status().unwrap().queued(), 1);

    let (gate_held_tx, gate_held_rx) = std::sync::mpsc::sync_channel(1);
    let (gate_release_tx, gate_release_rx) = std::sync::mpsc::sync_channel(1);
    let gate = observer.lifecycle_gate.clone();
    let gate_holder = std::thread::spawn(move || {
        let guard = gate.lock_from_taskvisor_callback();
        gate_held_tx
            .send(())
            .expect("the test must observe the held lifecycle gate");
        gate_release_rx
            .recv()
            .expect("the test must release the lifecycle gate");
        drop(guard);
    });
    gate_held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the lifecycle gate must be held");

    let canceled_finalizer = {
        let observer = Arc::clone(&observer);
        tokio::spawn(async move {
            observer
                .finalize_async(
                    finalizer_tv.get(),
                    Finalization {
                        phase: TaskPhase::Succeeded,
                        error: None,
                        exit_code: None,
                        force: true,
                        safe_without_barrier: true,
                    },
                )
                .await;
        })
    };
    wait_for_condition(
        "the canceled finalizer must own two permits while waiting asynchronously for the gate",
        || {
            state.persistence_status().unwrap().queued() == 3
                && state.active_persistence_admissions() == 1
                && observer.lifecycle_gate.waiters() == 1
        },
    )
    .await;
    canceled_finalizer.abort();
    assert!(canceled_finalizer.await.unwrap_err().is_cancelled());
    wait_for_condition(
        "canceling after admission must release both permits and the gate waiter",
        || {
            state.persistence_status().unwrap().queued() == 1
                && state.active_persistence_admissions() == 0
                && observer.lifecycle_gate.waiters() == 0
        },
    )
    .await;
    assert_eq!(state.tv_for(&finalizer_id), Some(finalizer_tv));

    let callback_event = Event::new(EventKind::TaskRemoved).with_id(callback_tv);
    let (callback_done_tx, callback_done_rx) = tokio::sync::oneshot::channel();
    let callback_observer = Arc::clone(&observer);
    let callback = std::thread::spawn(move || {
        callback_observer.on_event(&callback_event);
        let _ = callback_done_tx.send(());
    });
    wait_for_condition(
        "TaskRemoved must reserve its maximum finalization batch before the lifecycle gate",
        || {
            state.persistence_status().unwrap().queued() == 3
                && observer.lifecycle_gate.waiters() == 1
        },
    )
    .await;

    let mut finalizer = {
        let observer = Arc::clone(&observer);
        tokio::spawn(async move {
            observer
                .finalize_async(
                    finalizer_tv.get(),
                    Finalization {
                        phase: TaskPhase::Succeeded,
                        error: None,
                        exit_code: None,
                        force: true,
                        safe_without_barrier: true,
                    },
                )
                .await;
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(state.persistence_status().unwrap().queued(), 3);
    assert_eq!(observer.lifecycle_gate.waiters(), 1);

    let sink_release = tokio::spawn(async move {
        tokio::task::yield_now().await;
        sink_release_tx
            .send(())
            .expect("ordinary Tokio work must release the persistence callback");
    });
    gate_release_tx
        .send(())
        .expect("the lifecycle gate holder must still be waiting");

    sink_release.await.expect("sink release task must finish");
    let callback_completed = tokio::time::timeout(Duration::from_secs(2), callback_done_rx)
        .await
        .is_ok();
    let finalizer_completed = tokio::time::timeout(Duration::from_secs(2), &mut finalizer).await;
    if finalizer_completed.is_err() {
        finalizer.abort();
        let _ = finalizer.await;
    }
    assert!(
        callback_completed && finalizer_completed.is_ok(),
        "TaskRemoved and the async finalizer must not retain the lifecycle gate and persistence permits while waiting for each other"
    );
    finalizer_completed
        .expect("the async finalizer must finish")
        .expect("the async finalizer task must finish");
    callback.join().expect("the callback thread must finish");
    gate_holder
        .join()
        .expect("the lifecycle gate holder must finish");
    wait_for_condition("all persistence ownership must be released", || {
        state.persistence_status().unwrap().queued() == 0
    })
    .await;

    assert_eq!(
        state.get(&callback_id).unwrap().status().phase(),
        TaskPhase::Failed
    );
    assert!(state.tv_for(&callback_id).is_none());
    assert_eq!(
        state.get(&finalizer_id).unwrap().status().phase(),
        TaskPhase::Succeeded
    );
    assert!(state.tv_for(&finalizer_id).is_none());
    state.shutdown_persistence().await;
}

#[tokio::test]
async fn confirmed_shutdown_releases_waiter_error_without_task_removed_event() {
    let (sub, state, id) = setup("shutdown-missing-outcome");
    let tv = state.tv_for(&id).expect("bound task");

    sub.finalize_unavailable(tv.get(), "task outcome unavailable: shutting down".into())
        .await;
    sub.finalize_pending_after_confirmed_shutdown().await;

    assert!(state.tv_for(&id).is_none());
    let task = state.get(&id).expect("retained failed task");
    assert_eq!(task.status().phase(), TaskPhase::Failed);
    assert!(
        task.status()
            .error()
            .is_some_and(|error| error.contains("outcome unavailable"))
    );
}

#[test]
fn runtime_failures_are_diagnostic_only() {
    for (name, reason) in [
        ("remove-diagnostic", "remove_failed: registry closed"),
        ("future-diagnostic", "future_controller_diagnostic: detail"),
    ] {
        let (sub, state, id) = setup(name);
        let tv = state.tv_for(&id).expect("bound task");

        sub.on_event(
            &Event::new(EventKind::RuntimeFailure)
                .with_id(tv)
                .with_reason(reason),
        );

        assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
        assert_eq!(state.tv_for(&id), Some(tv));
    }
}

#[test]
fn typed_controller_rejections_project_state() {
    for (name, kind, reason, expected) in [
        (
            "drop-rejection",
            taskvisor::RejectionKind::SlotBusy,
            "slot is busy; this diagnostic text is not schema",
            TaskPhase::Canceled,
        ),
        (
            "add-rejection",
            taskvisor::RejectionKind::AdmissionFailed,
            "add_failed: command queue closed",
            TaskPhase::Failed,
        ),
        (
            "queue-start-rejection",
            taskvisor::RejectionKind::AdmissionFailed,
            "queue_start_failed: shutting down",
            TaskPhase::Failed,
        ),
        (
            "removed-rejection",
            taskvisor::RejectionKind::RemovedFromQueue,
            "removed_from_queue",
            TaskPhase::Canceled,
        ),
    ] {
        let (sub, state, id) = setup(name);
        let tv = state.tv_for(&id).expect("bound task");

        sub.on_event(
            &Event::new(EventKind::ControllerRejected)
                .with_id(tv)
                .with_rejection_kind(kind)
                .with_reason(reason),
        );

        assert_eq!(state.get(&id).unwrap().status().phase(), expected);
        assert_eq!(state.tv_for(&id), Some(tv));
    }
}

#[test]
fn task_add_failed_is_always_terminal_for_its_identity() {
    let (sub, state, id) = setup("registry-add-failed");
    let tv = state.tv_for(&id).expect("bound task");

    sub.on_event(
        &Event::new(EventKind::TaskAddFailed)
            .with_id(tv)
            .with_rejection_kind(taskvisor::RejectionKind::AdmissionFailed)
            .with_reason("future_registry_rejection"),
    );

    assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Failed);
    assert_eq!(state.tv_for(&id), Some(tv));
}

#[test]
fn task_removed_is_observability_only_for_current_and_stale_identities() {
    let (sub, state, id) = setup("reuse-x");
    let tvs = [
        taskvisor::TaskId::for_tests(),
        taskvisor::TaskId::for_tests(),
    ];
    bind_test_task(&state, &id, tvs[0]);
    bind_test_task(&state, &id, tvs[1]);

    let stale = Event::new(EventKind::TaskRemoved)
        .with_task("reuse-x")
        .with_id(tvs[0]);
    sub.on_event(&stale);
    assert!(
        state.get(&id).is_some(),
        "late TaskRemoved from the previous incarnation must be ignored"
    );

    let current = Event::new(EventKind::TaskRemoved)
        .with_task("reuse-x")
        .with_id(tvs[1]);
    sub.on_event(&current);
    assert!(
        state.get(&id).is_some(),
        "TaskRemoved must not bypass terminal-state retention"
    );
    assert_eq!(
        state.tv_for(&id).map(|tv| tv.get()),
        Some(tvs[1].get()),
        "the direct completion path remains the binding owner"
    );
}

#[test]
fn controller_rejection_projects_phase_but_waiter_owns_cleanup() {
    let state = TaskState::new();
    let id = add_test_task(&state, "rejected-task");
    let tv = taskvisor::TaskId::for_tests();
    bind_test_task(&state, &id, tv);
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));

    let ev = Event::new(EventKind::ControllerRejected)
        .with_task("some-slot")
        .with_id(tv)
        .with_rejection_kind(taskvisor::RejectionKind::QueueFull)
        .with_reason("queue_full: 3/3");
    sub.on_event(&ev);

    let task = state.get(&id).expect("entry kept for observability");
    assert_eq!(task.status().phase(), TaskPhase::Failed);
    assert!(
        task.status()
            .error()
            .is_some_and(|e| e.contains("queue_full")),
        "rejection reason must be recorded"
    );
    assert!(
        registry.subscribe_raw(&id).is_some(),
        "the event path must not race the waiter's output cleanup"
    );
    assert!(
        state.tv_for(&id).is_some(),
        "the binding stays owned until direct completion resolves"
    );
}

#[test]
fn terminal_events_become_sweepable_only_after_waiter_cleanup() {
    use crate::StateConfig;
    use std::time::Duration;

    let config = StateConfig::new()
        .with_run_ttl(Duration::ZERO)
        .with_task_ttl(Duration::ZERO);

    for (name, kind, phase, error, force) in [
        (
            "rej-reap",
            EventKind::ControllerRejected,
            TaskPhase::Failed,
            Some("queue_full: 3/3".into()),
            true,
        ),
        (
            "exh-reap",
            EventKind::TaskFinished,
            TaskPhase::Exhausted,
            None,
            false,
        ),
        (
            "dead-reap",
            EventKind::TaskFinished,
            TaskPhase::Failed,
            None,
            false,
        ),
    ] {
        let state = TaskState::new();
        let id = add_test_task(&state, name);
        let tv = taskvisor::TaskId::for_tests();
        let binding = bind_test_task(&state, &id, tv);
        let sub = RuntimeObserver::with_output_hub(
            state.clone(),
            Arc::new(OutputHub::new(OutputConfig::default())),
        );
        let event = match (kind, phase) {
            (EventKind::ControllerRejected, _) => Event::new(kind)
                .with_id(tv)
                .with_rejection_kind(taskvisor::RejectionKind::QueueFull)
                .with_reason("queue_full: 3/3"),
            (_, TaskPhase::Exhausted) => {
                assert!(state.transition_attempt_starting(&binding, 1));
                Event::new(kind)
                    .with_id(tv)
                    .with_outcome_kind(TaskOutcomeKind::Failed)
                    .with_reason("retry policy stopped after one retry")
            }
            (_, TaskPhase::Failed) => {
                assert!(state.transition_attempt_starting(&binding, 1));
                Event::new(kind)
                    .with_id(tv)
                    .with_outcome_kind(TaskOutcomeKind::Fatal)
                    .with_reason("fatal error (no retry): boom")
            }
            _ => unreachable!("test table contains only terminal event cases"),
        };

        sub.on_event(&event);
        assert_eq!(state.get(&id).unwrap().status().phase(), phase);
        assert!(state.tv_for(&id).is_some());
        assert_eq!(state.sweep_retention_for_test(&config).1, 0);
        assert_eq!(
            state.finalize_if_bound(tv.get(), phase, error, None, force),
            Some(id.clone()),
        );
        assert_eq!(state.sweep_retention_for_test(&config).1, 1);
        assert!(state.get(&id).is_none());
    }
}

#[test]
fn attempt_canceled_maps_to_canceled_phase() {
    let (sub, state, id) = setup("graceful");

    let ev = bound_event(&state, &id, EventKind::AttemptCanceled).with_attempt(1);
    sub.on_event(&ev);

    let task = state.get(&id).expect("task exists");
    assert_eq!(task.status().phase(), TaskPhase::Canceled);
}

#[test]
fn task_finished_fatal_preserves_optional_exit_code_and_waiter_cleanup_ownership() {
    for (name, reason, exit_code) in [
        ("fatal-task", "fatal error (no retry): boom", Some(137)),
        (
            "logical-fatal",
            "fatal error (no retry): misconfigured",
            None,
        ),
    ] {
        let (sub, state, registry, id) = setup_with_output_hub(name);
        let mut event = bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Fatal)
            .with_reason(reason);
        if let Some(exit_code) = exit_code {
            event = event.with_exit_code(exit_code);
        }

        sub.on_event(&event);

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), TaskPhase::Failed);
        assert_eq!(task.status().exit_code(), exit_code);
        assert_eq!(task.status().error(), Some(reason));
        assert!(task.status().phase().is_terminal());
        assert!(
            registry.subscribe_raw(&id).is_some(),
            "the direct completion path owns terminal channel eviction"
        );
    }
}

#[test]
fn runtime_failure_without_identity_does_not_touch_user_task() {
    let (sub, state, id) = setup("controller");

    let ev = Event::new(EventKind::RuntimeFailure)
        .with_task("controller")
        .with_reason("controller_loop_exited: boom");

    sub.on_event(&ev);

    assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
}

#[test]
fn attempt_failed_carries_event_exit_code_into_state() {
    let (sub, state, id) = setup("fail-task");

    let ev = bound_event(&state, &id, EventKind::AttemptFailed)
        .with_attempt(1)
        .with_reason("execution failed: non-zero")
        .with_exit_code(2);

    sub.on_event(&ev);

    let task = state.get(&id).expect("task exists");
    assert_eq!(task.status().phase(), TaskPhase::Failed);
    assert_eq!(task.status().exit_code(), Some(2));
}

#[test]
fn task_finished_failed_carries_event_exit_code_into_state() {
    let (sub, state, id) = setup("exhausted");

    let ev = bound_event(&state, &id, EventKind::TaskFinished)
        .with_outcome_kind(TaskOutcomeKind::Failed)
        .with_reason("retry limit reached after five retries")
        .with_exit_code(1);

    sub.on_event(&ev);

    let task = state.get(&id).expect("task exists");
    assert_eq!(task.status().phase(), TaskPhase::Exhausted);
    assert_eq!(task.status().exit_code(), Some(1));
}

fn setup_pending_with_output_hub(
    task_name: &str,
) -> (RuntimeObserver, TaskState, Arc<OutputHub>, TaskId) {
    let state = TaskState::new();
    let id = add_test_task(&state, task_name);
    bind_test_task(&state, &id, taskvisor::TaskId::for_tests());
    let registry = Arc::new(OutputHub::new(OutputConfig::try_new(16).unwrap()));
    registry.ensure_channel(id.clone());
    let sub = RuntimeObserver::with_output_hub(state.clone(), Arc::clone(&registry));
    (sub, state, registry, id)
}

fn setup_with_output_hub(task_name: &str) -> (RuntimeObserver, TaskState, Arc<OutputHub>, TaskId) {
    let setup = setup_pending_with_output_hub(task_name);
    let binding = setup.1.binding_for(&setup.3).expect("task must be bound");
    assert!(setup.1.transition_attempt_starting(&binding, 1));
    setup
}

#[test]
fn attempt_starting_announces_run_started_into_output_hub() {
    let (sub, state, registry, id) = setup_pending_with_output_hub("started-1");
    let mut rx = registry.subscribe_raw(&id).unwrap();

    let ev = bound_event(&state, &id, EventKind::AttemptStarting).with_attempt(1);
    sub.on_event(&ev);

    match rx.try_recv().unwrap() {
        OutputEvent::RunStarted {
            generation,
            attempt,
            ..
        } => {
            assert_eq!(generation, 1);
            assert_eq!(attempt, 1);
        }
        other => panic!("expected RunStarted, got {other:?}"),
    }
}

#[test]
fn attempt_succeeded_announces_run_finished_with_no_exit_code() {
    let (sub, state, registry, id) = setup_with_output_hub("stopped-1");
    let mut rx = registry.subscribe_raw(&id).unwrap();

    let ev = bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1);
    sub.on_event(&ev);

    match rx.try_recv().unwrap() {
        OutputEvent::RunFinished {
            generation,
            attempt,
            exit_code,
            ..
        } => {
            assert_eq!(generation, 1);
            assert_eq!(attempt, 1);
            assert_eq!(exit_code, None);
        }
        other => panic!("expected RunFinished, got {other:?}"),
    }
}

#[test]
fn duplicate_attempt_succeeded_does_not_announce_a_second_run_finished() {
    let (sub, state, registry, id) = setup_with_output_hub("stopped-duplicate");
    let mut rx = registry.subscribe_raw(&id).unwrap();
    let event = bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1);

    sub.on_event(&event);
    sub.on_event(&event);

    assert!(matches!(
        rx.try_recv(),
        Ok(OutputEvent::RunFinished { attempt: 1, .. })
    ));
    assert!(
        rx.try_recv().is_err(),
        "a duplicate terminal attempt event must be an exact no-op"
    );
}

#[test]
fn duplicate_old_generation_terminal_does_not_announce_again_after_apply() {
    let (sub, state, registry, id) = setup_with_output_hub("stopped-old-generation");
    let mut rx = registry.subscribe_raw(&id).unwrap();
    let event = bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1);

    sub.on_event(&event);
    assert!(matches!(
        rx.try_recv(),
        Ok(OutputEvent::RunFinished {
            generation: 1,
            attempt: 1,
            ..
        })
    ));

    state
        .apply_desired(&TaskManifest::new(id.clone(), changed_test_spec()).unwrap())
        .unwrap();
    sub.on_event(&event);

    assert!(
        rx.try_recv().is_err(),
        "a duplicate terminal event from the previous generation must remain a no-op"
    );
}

#[test]
fn attempt_failed_announces_run_finished_with_exit_code() {
    let (sub, state, registry, id) = setup_with_output_hub("failed-1");
    let mut rx = registry.subscribe_raw(&id).unwrap();

    let ev = bound_event(&state, &id, EventKind::AttemptFailed)
        .with_attempt(1)
        .with_exit_code(17);
    sub.on_event(&ev);

    match rx.try_recv().unwrap() {
        OutputEvent::RunFinished {
            generation,
            attempt,
            exit_code,
            ..
        } => {
            assert_eq!(generation, 1);
            assert_eq!(attempt, 1);
            assert_eq!(exit_code, Some(17));
        }
        other => panic!("expected RunFinished, got {other:?}"),
    }
}

#[test]
fn task_finished_refines_state_without_duplicate_run_finished() {
    let (sub, state, registry, id) = setup_with_output_hub("exh-evict");
    let mut rx = registry.subscribe_raw(&id).unwrap();

    sub.on_event(
        &bound_event(&state, &id, EventKind::AttemptFailed)
            .with_attempt(1)
            .with_exit_code(1),
    );
    sub.on_event(
        &bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Failed)
            .with_reason("retry policy stopped")
            .with_exit_code(1),
    );

    match rx.try_recv().unwrap() {
        OutputEvent::RunFinished {
            generation,
            attempt,
            ..
        } => {
            assert_eq!(generation, 1);
            assert_eq!(attempt, 1);
        }
        other => panic!("expected RunFinished, got {other:?}"),
    }
    assert!(
        rx.try_recv().is_err(),
        "TaskFinished is task-level and must not announce a second RunFinished"
    );
    assert!(
        registry.subscribe_raw(&id).is_some(),
        "the direct completion path owns terminal channel eviction"
    );
}

#[test]
fn attempt_timed_out_is_a_single_terminal_attempt_event() {
    let (sub, state, registry, id) = setup_with_output_hub("slow-task");
    let mut rx = registry.subscribe_raw(&id).unwrap();

    sub.on_event(
        &bound_event(&state, &id, EventKind::AttemptTimedOut)
            .with_attempt(1)
            .with_timeout(Duration::from_millis(250)),
    );

    let task = state.get(&id).expect("task exists");
    assert_eq!(task.status().phase(), TaskPhase::Timeout);
    assert_eq!(
        task.status().error(),
        Some("task attempt timed out after 250 ms"),
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(OutputEvent::RunFinished { attempt: 1, .. })
    ));
}

#[test]
fn task_finished_canceled_maps_by_kind_not_reason() {
    let (sub, state, id) = setup("self-cancel");

    sub.on_event(
        &bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Canceled)
            .with_reason("text that must not select a phase"),
    );

    let task = state.get(&id).expect("task exists");
    assert_eq!(
        task.status().phase(),
        TaskPhase::Canceled,
        "the typed outcome selects cancellation"
    );
    assert!(task.status().error().is_none());
}

#[test]
fn task_finished_runtime_failures_use_typed_outcomes() {
    for (name, kind, expected_phase, expected_error) in [
        (
            "force-aborted",
            TaskOutcomeKind::ForceAborted,
            TaskPhase::Canceled,
            crate::map::phase::FORCE_ABORTED_ERROR,
        ),
        (
            "runner-panicked",
            TaskOutcomeKind::Panicked,
            TaskPhase::Failed,
            crate::map::phase::TASK_RUNNER_PANICKED_ERROR,
        ),
    ] {
        let (sub, state, id) = setup(name);

        sub.on_event(
            &bound_event(&state, &id, EventKind::TaskFinished)
                .with_outcome_kind(kind)
                .with_reason("diagnostic text that must not select the phase"),
        );

        let task = state.get(&id).expect("task exists");
        assert_eq!(task.status().phase(), expected_phase);
        assert_eq!(task.status().error(), Some(expected_error));
    }
}

#[test]
fn task_finished_completed_after_success_is_not_an_error() {
    let (sub, state, id) = setup("oneshot");

    sub.on_event(&bound_event(&state, &id, EventKind::AttemptSucceeded).with_attempt(1));
    sub.on_event(
        &bound_event(&state, &id, EventKind::TaskFinished)
            .with_outcome_kind(TaskOutcomeKind::Completed)
            .with_reason("diagnostic text that looks like a failure"),
    );

    let task = state.get(&id).expect("task exists");
    assert_eq!(
        task.status().phase(),
        TaskPhase::Succeeded,
        "normal one-shot completion must stay Succeeded"
    );
    assert!(
        task.status().error().is_none(),
        "Completed ignores diagnostic reason text"
    );
}

#[test]
fn task_finished_without_outcome_kind_does_not_guess_from_reason() {
    let (sub, state, id) = setup("missing-kind");

    sub.on_event(
        &bound_event(&state, &id, EventKind::TaskFinished).with_reason("fatal-looking diagnostic"),
    );

    assert_eq!(state.get(&id).unwrap().status().phase(), TaskPhase::Running);
}

#[test]
fn task_removed_does_not_bypass_waiter_cleanup_or_retention() {
    let (sub, state, registry, id) = setup_with_output_hub("remove");

    let ev = bound_event(&state, &id, EventKind::TaskRemoved);
    sub.on_event(&ev);

    assert!(
        registry.subscribe_raw(&id).is_some(),
        "TaskRemoved must not race the waiter's output cleanup"
    );
    assert!(state.get(&id).is_some(), "task_ttl retention stays intact");
    assert!(state.tv_for(&id).is_some(), "waiter still owns the binding");
}
