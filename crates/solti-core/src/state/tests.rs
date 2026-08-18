use std::{
    cell::Cell,
    pin::Pin,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

use solti_model::{
    Annotations, ConditionStatus, EmbeddedSpec, Flag, LabelSelector, Labels, SubprocessMode,
    SubprocessSpec, TaskEnv, TaskManifest, TaskSpec, TaskWorkload,
};
use tokio_stream::StreamExt;

use super::*;

fn spec(slot: &str, timeout_ms: u64) -> TaskSpec {
    TaskSpec::builder(
        slot,
        TaskWorkload::Embedded(EmbeddedSpec::new("test-v1").unwrap()),
        timeout_ms,
    )
    .build()
    .expect("valid test spec")
}

fn manifest(name: &str, slot: &str, timeout_ms: u64) -> TaskManifest {
    TaskManifest::new(name, spec(slot, timeout_ms)).expect("valid test Task manifest")
}

fn annotated_manifest(name: &str, annotation_bytes: usize) -> TaskManifest {
    let mut annotations = Annotations::new();
    annotations.insert("example.io/payload", "x".repeat(annotation_bytes));
    manifest(name, "slot", 1_000)
        .with_annotations(annotations)
        .expect("valid annotated test Task manifest")
}

fn create(state: &TaskState, name: &str) -> Task {
    state
        .create_desired(&manifest(name, "slot", 1_000))
        .expect("create")
        .task
}

fn journal_task(name: &str, revision: u64, annotation_bytes: usize) -> Task {
    let mut task_manifest = manifest(name, "slot", 1_000);
    if annotation_bytes > 0 {
        let mut annotations = Annotations::new();
        annotations.insert("example.io/payload", "x".repeat(annotation_bytes));
        task_manifest = task_manifest.with_annotations(annotations).unwrap();
    }
    let mut task = Task::from_manifest(task_manifest).unwrap();
    task.set_resource_version(format!("epoch:{revision}"))
        .unwrap();
    task
}

fn record_current_change(state: &TaskState, task: Task) {
    let mut inner = state.write(StateMutationEventCapacity::TaskChange);
    let (revision, resource_version) = TaskState::next_resource_version(&mut inner);
    assert_eq!(task.metadata().resource_version(), resource_version);
    state.record_change(&mut inner, revision, None, Some(Arc::new(task)));
}

fn run_journal_insertion(name: &str, attempt: u32) -> RawRunChange {
    let task = TaskId::new(name).unwrap();
    let task_uid = Uid::new("run-journal-test-uid").unwrap();
    let run = Arc::new(
        TaskRun::starting(
            1,
            attempt,
            WorkloadTypeMeta::new("example.io/v1", "Example").unwrap(),
        )
        .unwrap(),
    );
    TaskState::run_snapshot_change(&task, &task_uid, None, Some(run))
}

fn bind(state: &TaskState, name: &TaskId) -> RuntimeBinding {
    let resource = ResourceGeneration::from_task(&state.get(name).expect("resource must exist"));
    let tv = taskvisor::TaskId::for_tests();
    assert!(state.bind_tv(resource.clone(), tv));
    RuntimeBinding { resource, tv }
}

#[test]
fn try_new_creates_empty_state() {
    let state = TaskState::try_new().expect("OS entropy is available");

    assert!(state.list_all().is_empty());
}

#[test]
fn maximum_watch_history_capacity_has_constant_initial_allocation() {
    let config = StateConfig::new()
        .try_with_watch_history_capacity(usize::MAX)
        .unwrap();
    let state = TaskState::with_epoch(config, "maximum-capacity".to_string());
    let inner = state.inner.read();

    assert_eq!(inner.watch_history_capacity, usize::MAX);
    assert!(inner.watch_history.is_empty());
    assert_eq!(inner.watch_tx.receiver_count(), 0);
}

#[test]
fn live_watch_journal_lookup_has_a_logarithmic_comparison_bound() {
    let mut history = (1..=65_536_u64).collect::<VecDeque<_>>();
    for revision in 65_537..=98_304 {
        history.pop_front();
        history.push_back(revision);
    }
    let comparisons = Cell::new(0_usize);

    let change = first_after_revision(&history, 90_000, |revision| {
        comparisons.set(comparisons.get() + 1);
        *revision
    });

    assert_eq!(change, Some(&90_001));
    let logarithmic_bound = (usize::BITS - (history.len() - 1).leading_zeros()) as usize + 1;
    assert!(comparisons.get() <= logarithmic_bound);
    assert!(comparisons.get() < history.len() / 1_000);
}

#[tokio::test]
async fn no_sink_shutdown_waits_for_a_preclose_admission_to_commit() {
    let state = TaskState::new();
    let admission = state
        .admit_state_write(StateMutationEventCapacity::TaskChange)
        .await
        .expect("the pre-shutdown state admission must be accepted");
    assert_eq!(state.active_persistence_admissions(), 1);

    let shutdown_state = state.clone();
    let shutdown = tokio::spawn(async move {
        shutdown_state.shutdown_persistence().await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.persistence_admission_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown must close state mutation admission");
    assert!(!shutdown.is_finished());
    assert_eq!(state.active_persistence_admissions(), 1);

    let committed = state
        .create_desired_admitted(&manifest("preclose-commit", "slot", 1_000), admission)
        .expect("a pre-boundary admission may finish its commit");
    assert_eq!(committed.task.name().as_str(), "preclose-commit");
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown must finish after the pre-boundary lease is released")
        .expect("the shutdown task must not panic");

    assert_eq!(state.active_persistence_admissions(), 0);
    assert!(state.get(committed.task.name()).is_some());
}

#[derive(Default)]
struct RecordingStateSink {
    events: std::sync::Mutex<Vec<TaskStateEvent>>,
}

impl crate::TaskStateSink for RecordingStateSink {
    fn on_event(&self, event: &TaskStateEvent) {
        self.events.lock().unwrap().push(event.clone());
    }
}

struct BlockingStateSink {
    events: std::sync::Mutex<Vec<String>>,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    calls: AtomicUsize,
}

impl crate::TaskStateSink for BlockingStateSink {
    fn on_event(&self, event: &TaskStateEvent) {
        let TaskStateEvent::TaskChanged {
            resource_version, ..
        } = event
        else {
            return;
        };
        self.events.lock().unwrap().push(resource_version.clone());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
    }
}

struct SweepBackpressureStateSink {
    setup_events: AtomicUsize,
    setup_event_target: usize,
    setup_complete: std::sync::mpsc::SyncSender<()>,
    deletion_events: AtomicUsize,
    deletion_entered: std::sync::mpsc::SyncSender<()>,
    deletion_release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl crate::TaskStateSink for SweepBackpressureStateSink {
    fn on_event(&self, event: &TaskStateEvent) {
        if matches!(event, TaskStateEvent::TaskChanged { current: None, .. }) {
            if self.deletion_events.fetch_add(1, Ordering::SeqCst) == 0 {
                self.deletion_entered.send(()).unwrap();
                self.deletion_release.lock().unwrap().recv().unwrap();
            }
            return;
        }

        if self.setup_events.fetch_add(1, Ordering::SeqCst) + 1 == self.setup_event_target {
            self.setup_complete.send(()).unwrap();
        }
    }
}

struct ArmableStateSink {
    events: AtomicUsize,
    setup_complete: std::sync::mpsc::SyncSender<()>,
    block_next: AtomicBool,
    entered: std::sync::mpsc::SyncSender<()>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl crate::TaskStateSink for ArmableStateSink {
    fn on_event(&self, _event: &TaskStateEvent) {
        let should_block = self.block_next.swap(false, Ordering::SeqCst);
        if self.events.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            self.setup_complete.send(()).unwrap();
        }
        if should_block {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_state_sink_does_not_hold_state_lock_and_preserves_commit_order() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(BlockingStateSink {
        events: std::sync::Mutex::new(Vec::new()),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
        calls: AtomicUsize::new(0),
    });
    let state = TaskState::with_epoch_and_sink(
        StateConfig::default(),
        "ordered-sink".to_string(),
        Some(sink.clone()),
    );

    let first_state = state.clone();
    let first = std::thread::spawn(move || {
        first_state.add_task(manifest("first", "slot", 1_000));
    });
    let entered = entered_rx.recv_timeout(Duration::from_secs(5)).is_ok();

    let (second_done_tx, second_done_rx) = std::sync::mpsc::sync_channel(1);
    let second_state = state.clone();
    let second = std::thread::spawn(move || {
        second_state.add_task(manifest("second", "slot", 1_000));
        second_done_tx.send(()).unwrap();
    });
    let second_finished_before_release =
        second_done_rx.recv_timeout(Duration::from_secs(1)).is_ok();

    release_tx.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    state.shutdown_persistence().await;

    assert!(entered, "the first committed event must reach the sink");
    assert!(
        second_finished_before_release,
        "a blocked sink must not retain the TaskState write lock"
    );
    assert_eq!(
        *sink.events.lock().unwrap(),
        ["ordered-sink:1", "ordered-sink:2"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_state_persistence_queue_applies_backpressure_without_dropping() {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(BlockingStateSink {
        events: std::sync::Mutex::new(Vec::new()),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
        calls: AtomicUsize::new(0),
    });
    let state = TaskState::try_with_epoch_and_sink(
        StateConfig::default(),
        "bounded-sink".to_string(),
        Some(sink.clone()),
        PersistenceConfig::new()
            .try_with_state_queue_capacity(2)
            .unwrap(),
    )
    .unwrap();

    state.add_task(manifest("first", "slot", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("worker must block in the first callback");
    state.add_task(manifest("second", "slot", 1_000));
    state.add_task(manifest("third", "slot", 1_000));

    let (fourth_done_tx, fourth_done_rx) = std::sync::mpsc::sync_channel(1);
    let fourth_state = state.clone();
    let fourth = std::thread::spawn(move || {
        fourth_state.add_task(manifest("fourth", "slot", 1_000));
        fourth_done_tx.send(()).unwrap();
    });
    assert!(
        fourth_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "the fourth commit must wait while the two-event queue is full"
    );

    release_tx.send(()).unwrap();
    fourth_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("backpressure must release after the sink advances");
    fourth.join().unwrap();
    state.shutdown_persistence().await;

    assert_eq!(
        *sink.events.lock().unwrap(),
        [
            "bounded-sink:1",
            "bounded-sink:2",
            "bounded-sink:3",
            "bounded-sink:4"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn maximum_event_commit_reserves_all_permits_before_the_state_lock() {
    let (setup_complete_tx, setup_complete_rx) = std::sync::mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(ArmableStateSink {
        events: AtomicUsize::new(0),
        setup_complete: setup_complete_tx,
        block_next: AtomicBool::new(false),
        entered: entered_tx,
        release: std::sync::Mutex::new(release_rx),
    });
    let state = TaskState::try_with_epoch_and_sink(
        StateConfig::default(),
        "atomic-reservation".to_string(),
        Some(sink.clone()),
        PersistenceConfig::new()
            .try_with_state_queue_capacity(2)
            .unwrap(),
    )
    .unwrap();

    let task = create(&state, "attempt");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));
    setup_complete_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the three setup events must drain");

    sink.block_next.store(true, Ordering::SeqCst);
    state.add_task(manifest("active-callback", "slot", 1_000));
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the armed callback must block");
    state.add_task(manifest("buffered", "slot", 1_000));

    let (transition_done_tx, transition_done_rx) = std::sync::mpsc::sync_channel(1);
    let transition_state = state.clone();
    let transition_binding = binding.clone();
    let transition = std::thread::spawn(move || {
        transition_done_tx
            .send(transition_state.transition_attempt_starting(&transition_binding, 2))
            .unwrap();
    });
    assert!(
        transition_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "a three-event commit must wait until all three permits are available"
    );
    assert_eq!(state.get(task.name()).unwrap().status().attempt(), 1);
    assert!(
        state.event_publisher.inner.lock().pending.is_empty(),
        "a commit waiting for permits must not create a hidden pending batch"
    );

    release_tx.send(()).unwrap();
    assert!(
        transition_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the transition must proceed after all permits are released")
    );
    transition.join().unwrap();
    state.shutdown_persistence().await;

    assert_eq!(state.get(task.name()).unwrap().status().attempt(), 2);
    assert_eq!(sink.events.load(Ordering::SeqCst), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sweep_deletions_apply_event_level_backpressure() {
    let (setup_complete_tx, setup_complete_rx) = std::sync::mpsc::sync_channel(1);
    let (deletion_entered_tx, deletion_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (deletion_release_tx, deletion_release_rx) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(SweepBackpressureStateSink {
        setup_events: AtomicUsize::new(0),
        setup_event_target: 8,
        setup_complete: setup_complete_tx,
        deletion_events: AtomicUsize::new(0),
        deletion_entered: deletion_entered_tx,
        deletion_release: std::sync::Mutex::new(deletion_release_rx),
    });
    let state = TaskState::try_with_epoch_and_sink(
        StateConfig::default(),
        "bounded-sweep".to_string(),
        Some(sink.clone()),
        PersistenceConfig::new()
            .try_with_state_queue_capacity(2)
            .unwrap(),
    )
    .unwrap();

    for index in 0..4 {
        let task = create(&state, &format!("expired-{index}"));
        let binding = bind(&state, task.name());
        assert_eq!(
            state.finalize_if_bound(
                binding.tv.get(),
                TaskPhase::Canceled,
                Some("canceled".into()),
                None,
                true,
            ),
            Some(task.name().clone())
        );
    }
    setup_complete_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("all setup events must drain before the sweep");

    let (sweep_done_tx, sweep_done_rx) = std::sync::mpsc::sync_channel(1);
    let sweep_state = state.clone();
    let sweep = std::thread::spawn(move || {
        let config = StateConfig::new()
            .with_run_ttl(Duration::ZERO)
            .with_task_ttl(Duration::ZERO);
        sweep_done_tx.send(sweep_state.sweep(&config)).unwrap();
    });
    deletion_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the first deletion must reach the persistence worker");
    assert!(
        sweep_done_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "four deletion events must not fit in one active plus two buffered event slots"
    );

    deletion_release_tx.send(()).unwrap();
    assert_eq!(
        sweep_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the sweep must finish after persistence advances"),
        (0, 4)
    );
    sweep.join().unwrap();
    state.shutdown_persistence().await;

    assert_eq!(sink.deletion_events.load(Ordering::SeqCst), 4);
}

struct ReadinessBarrierStateSink {
    state_inner: std::sync::Mutex<Option<Arc<RwLock<TaskStateInner>>>>,
    events: std::sync::Mutex<Vec<String>>,
    first_entered: std::sync::mpsc::SyncSender<()>,
    first_release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    second_entered: std::sync::mpsc::SyncSender<()>,
    second_callback_unlocked: AtomicBool,
    calls: AtomicUsize,
}

impl crate::TaskStateSink for ReadinessBarrierStateSink {
    fn on_event(&self, event: &TaskStateEvent) {
        let TaskStateEvent::TaskChanged {
            resource_version, ..
        } = event
        else {
            return;
        };
        self.events.lock().unwrap().push(resource_version.clone());
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.first_entered.send(()).unwrap();
                self.first_release.lock().unwrap().recv().unwrap();
            }
            1 => {
                let state_inner = self
                    .state_inner
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("test state must be installed before publishing");
                self.second_callback_unlocked
                    .store(state_inner.try_write().is_some(), Ordering::SeqCst);
                self.second_entered.send(()).unwrap();
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_sink_waits_for_enqueuing_write_guard_to_release() {
    let (first_entered_tx, first_entered_rx) = std::sync::mpsc::sync_channel(1);
    let (first_release_tx, first_release_rx) = std::sync::mpsc::sync_channel(1);
    let (second_entered_tx, second_entered_rx) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(ReadinessBarrierStateSink {
        state_inner: std::sync::Mutex::new(None),
        events: std::sync::Mutex::new(Vec::new()),
        first_entered: first_entered_tx,
        first_release: std::sync::Mutex::new(first_release_rx),
        second_entered: second_entered_tx,
        second_callback_unlocked: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let state = TaskState::with_epoch_and_sink(
        StateConfig::default(),
        "ready-sink".to_string(),
        Some(sink.clone()),
    );
    *sink.state_inner.lock().unwrap() = Some(Arc::clone(&state.inner));

    let (first_done_tx, first_done_rx) = std::sync::mpsc::sync_channel(1);
    let first_state = state.clone();
    let first = std::thread::spawn(move || {
        first_state.add_task(manifest("first", "slot", 1_000));
        first_done_tx.send(()).unwrap();
    });
    first_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first callback must enter");

    let (second_enqueued_tx, second_enqueued_rx) = std::sync::mpsc::sync_channel(1);
    let (second_release_tx, second_release_rx) = std::sync::mpsc::sync_channel(1);
    let second_state = state.clone();
    let second = std::thread::spawn(move || {
        let manifest = manifest("second", "slot", 1_000);
        let manifest_bytes = TaskState::serialized_task_manifest_bytes(&manifest);
        let mut task = Task::from_manifest(manifest).unwrap();
        let mut inner = second_state.write(StateMutationEventCapacity::TaskChange);
        let (revision, resource_version) = TaskState::next_resource_version(&mut inner);
        task.set_resource_version(resource_version).unwrap();
        TaskState::index_task(&mut inner, &task);
        let task = Arc::new(task);
        inner.tasks.insert(task.name().clone(), Arc::clone(&task));
        TaskState::set_retained_task_manifest_bytes(&mut inner, task.name(), manifest_bytes);
        second_state.record_change(&mut inner, revision, None, Some(task));
        second_enqueued_tx.send(()).unwrap();
        second_release_rx.recv().unwrap();
    });
    second_enqueued_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second event must enqueue while its write guard is held");

    first_release_tx.send(()).unwrap();
    first_done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first publisher must stop at the unready second event");
    let second_entered_before_release = match second_entered_rx.try_recv() {
        Ok(()) => true,
        Err(std::sync::mpsc::TryRecvError::Empty) => false,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            panic!("second callback channel disconnected")
        }
    };

    second_release_tx.send(()).unwrap();
    let second_event_delivered = second_entered_before_release
        || second_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .is_ok();
    first.join().unwrap();
    second.join().unwrap();
    state.shutdown_persistence().await;

    let second_callback_unlocked = sink.second_callback_unlocked.load(Ordering::SeqCst);
    let events = sink.events.lock().unwrap().clone();
    *sink.state_inner.lock().unwrap() = None;
    assert!(
        !second_entered_before_release,
        "an event must remain unready until its state write guard releases"
    );
    assert!(second_event_delivered);
    assert!(second_callback_unlocked);
    assert_eq!(events, ["ready-sink:1", "ready-sink:2"]);
}

struct ReentrantStateSink {
    state: std::sync::Mutex<Option<TaskState>>,
    attempted: AtomicBool,
}

impl crate::TaskStateSink for ReentrantStateSink {
    fn on_event(&self, event: &TaskStateEvent) {
        if !matches!(event, TaskStateEvent::TaskChanged { .. }) {
            return;
        }

        let state = self
            .state
            .lock()
            .unwrap()
            .clone()
            .expect("test state must be installed before publishing");
        assert!(state.inner.try_read().is_some());
        self.attempted.store(true, Ordering::SeqCst);
        state.add_task(manifest("nested", "slot", 1_000));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_sink_mutation_is_rejected_before_nested_commit() {
    let sink = Arc::new(ReentrantStateSink {
        state: std::sync::Mutex::new(None),
        attempted: AtomicBool::new(false),
    });
    let state = TaskState::with_epoch_and_sink(
        StateConfig::default(),
        "reentrant-sink".to_string(),
        Some(sink.clone()),
    );
    *sink.state.lock().unwrap() = Some(state.clone());

    state.add_task(manifest("outer", "slot", 1_000));
    state.shutdown_persistence().await;

    let nested_exists = state.contains_task(&TaskId::new("nested").unwrap());
    *sink.state.lock().unwrap() = None;

    assert!(sink.attempted.load(Ordering::SeqCst));
    assert!(!nested_exists);
}

#[tokio::test]
async fn state_sink_receives_task_and_run_lifecycle() {
    let recording = Arc::new(RecordingStateSink::default());
    let sink: crate::TaskStateSinkHandle = recording.clone();
    let state = TaskState::with_epoch_and_sink(
        StateConfig::default(),
        "sink-epoch".to_string(),
        Some(sink),
    );

    let created = create(&state, "sink-task");
    let name = created.name().clone();

    let mut desired = manifest("sink-task", "slot", 2_000);
    let mut annotations = Annotations::new();
    annotations.insert("example.io/revision", "two");
    desired = desired.with_annotations(annotations).unwrap();
    state.apply_desired(&desired).expect("apply");

    let binding = bind(&state, &name);
    assert!(state.mark_observed(&binding.resource));
    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));
    assert!(state.delete_task(&name));
    state.shutdown_persistence().await;

    let events = recording.events.lock().unwrap();
    let task_changes = events
        .iter()
        .filter(|event| matches!(event, TaskStateEvent::TaskChanged { .. }))
        .count();
    let runs: Vec<&TaskRun> = events
        .iter()
        .filter_map(|event| match event {
            TaskStateEvent::RunChanged {
                task,
                task_uid,
                run,
            } if task == &name && task_uid == created.uid() => Some(run),
            _ => None,
        })
        .collect();

    assert_eq!(task_changes, 6);
    assert_eq!(runs.len(), 2);
    assert!(runs[0].is_active());
    assert_eq!(runs[1].phase(), TaskPhase::Succeeded);
    assert!(matches!(
        events.last(),
        Some(TaskStateEvent::TaskChanged {
            previous: Some(task),
            current: None,
            ..
        }) if task.name() == &name
    ));
}

#[test]
fn create_materializes_server_owned_fields_and_preserves_user_owned_fields() {
    let state = TaskState::new();
    let mut labels = Labels::new();
    labels.insert("team", "runtime");
    let mut annotations = Annotations::new();
    annotations.insert("example.io/note", "kept");

    let incoming = manifest("server-owned", "slot", 1_000)
        .with_labels(labels.clone())
        .unwrap()
        .with_annotations(annotations.clone())
        .unwrap();

    let stored = state.create_desired(&incoming).unwrap().task;
    assert!(!stored.uid().as_str().is_empty());
    assert!(!stored.metadata().resource_version().is_empty());
    assert_eq!(stored.metadata().generation(), 1);
    assert_eq!(stored.status().phase(), TaskPhase::Pending);
    assert_eq!(stored.status().attempt(), 0);
    assert_eq!(stored.metadata().labels(), &labels);
    assert_eq!(stored.metadata().annotations(), &annotations);
}

#[test]
fn retained_manifest_measurement_is_exact_compact_json() {
    let manifest = annotated_manifest("measured", 1_024);

    assert_eq!(
        TaskState::serialized_task_manifest_bytes(&manifest),
        serde_json::to_vec(&manifest).unwrap().len()
    );
}

#[test]
fn create_conflicts_with_every_retained_name_including_terminal() {
    let state = TaskState::new();
    let task = create(&state, "retained");
    let binding = bind(&state, task.name());
    assert_eq!(
        state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(task.name().clone())
    );

    let error = state
        .create_desired(&manifest("retained", "slot", 1_000))
        .unwrap_err();
    assert!(matches!(error, CoreError::AlreadyExists(_)));
}

#[test]
fn retained_task_limit_accepts_exactly_the_configured_count() {
    let config = StateConfig::new().try_with_max_retained_tasks(2).unwrap();
    let state = TaskState::with_epoch(config, "task-limit".to_string());
    let first = create(&state, "first-retained");
    create(&state, "second-retained");

    assert!(matches!(
        state.create_desired(&manifest("third-rejected", "slot", 1_000)),
        Err(CoreError::RetainedTaskLimitReached { limit: 2 })
    ));

    let binding = bind(&state, first.name());
    assert_eq!(
        state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(first.name().clone())
    );
    assert!(matches!(
        state.create_desired(&manifest("terminal-does-not-release", "slot", 1_000)),
        Err(CoreError::RetainedTaskLimitReached { limit: 2 })
    ));
    assert_eq!(state.list_all().len(), 2);
}

#[test]
fn unbounded_retained_task_config_disables_admission() {
    let config = StateConfig::new()
        .try_with_max_retained_tasks(2)
        .unwrap()
        .with_max_retained_tasks(None);
    let state = TaskState::with_epoch(config, "unbounded-tasks".to_string());

    create(&state, "unbounded-first");
    create(&state, "unbounded-second");
    create(&state, "unbounded-third");

    assert_eq!(state.list_all().len(), 3);
}

#[test]
fn retained_manifest_budget_accepts_the_exact_aggregate_and_counts_terminal_tasks() {
    let first = manifest("byte-first", "slot", 1_000);
    let second = annotated_manifest("byte-second", 128);
    let rejected = manifest("byte-rejected", "slot", 1_000);
    let first_bytes = TaskState::serialized_task_manifest_bytes(&first);
    let second_bytes = TaskState::serialized_task_manifest_bytes(&second);
    let rejected_bytes = TaskState::serialized_task_manifest_bytes(&rejected);
    let limit = first_bytes.checked_add(second_bytes).unwrap();
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(limit)
        .unwrap();
    let state = TaskState::with_epoch(config, "manifest-limit".to_string());

    let first = state.create_desired(&first).unwrap().task;
    state.create_desired(&second).unwrap();
    {
        let inner = state.inner.read();
        assert_eq!(inner.retained_task_manifest_bytes, limit);
        assert_eq!(inner.retained_task_manifest_bytes_by_name.len(), 2);
    }

    let binding = bind(&state, first.name());
    assert_eq!(
        state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(first.name().clone())
    );
    assert_eq!(state.inner.read().retained_task_manifest_bytes, limit);

    assert!(matches!(
        state.create_desired(&rejected),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded {
            current,
            requested,
            limit: actual_limit,
        }) if current == limit && requested == rejected_bytes && actual_limit == limit
    ));
}

#[test]
fn create_rejects_a_manifest_one_byte_over_the_budget() {
    let manifest = manifest("one-byte-over", "slot", 1_000);
    let requested = TaskState::serialized_task_manifest_bytes(&manifest);
    let limit = requested - 1;
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(limit)
        .unwrap();
    let state = TaskState::with_epoch(config, "one-byte-over".to_string());

    assert!(matches!(
        state.create_desired(&manifest),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded {
            current: 0,
            requested: actual_requested,
            limit: actual_limit,
        }) if actual_requested == requested && actual_limit == limit
    ));
    assert!(state.list_all().is_empty());
    assert_eq!(state.inner.read().resource_version, 0);
}

#[test]
fn unbounded_retained_manifest_budget_disables_byte_admission() {
    let manifests = [
        annotated_manifest("unbounded-byte-a", 64),
        annotated_manifest("unbounded-byte-b", 128),
        annotated_manifest("unbounded-byte-c", 256),
    ];
    let expected = manifests
        .iter()
        .map(TaskState::serialized_task_manifest_bytes)
        .sum::<usize>();
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .with_max_retained_task_manifest_bytes(None);
    let state = TaskState::with_epoch(config, "unbounded-manifests".to_string());

    for manifest in &manifests {
        state.create_desired(manifest).unwrap();
    }

    assert_eq!(state.inner.read().retained_task_manifest_bytes, expected);
}

#[test]
fn duplicate_name_precedes_the_retained_task_limit() {
    let config = StateConfig::new().try_with_max_retained_tasks(1).unwrap();
    let state = TaskState::with_epoch(config, "duplicate-limit".to_string());
    create(&state, "retained-name");

    assert!(matches!(
        state.create_desired(&manifest("retained-name", "slot", 1_000)),
        Err(CoreError::AlreadyExists(_))
    ));
}

#[test]
fn duplicate_and_count_admission_precede_the_manifest_byte_budget() {
    let first = manifest("precedence", "slot", 1_000);
    let first_bytes = TaskState::serialized_task_manifest_bytes(&first);
    let config = StateConfig::new()
        .try_with_max_retained_tasks(1)
        .unwrap()
        .try_with_max_retained_task_manifest_bytes(first_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "byte-precedence".to_string());
    state.create_desired(&first).unwrap();

    let duplicate = annotated_manifest("precedence", 1_024);
    assert!(matches!(
        state.create_desired(&duplicate),
        Err(CoreError::AlreadyExists(_))
    ));
    assert!(matches!(
        state.create_desired(&manifest("new-name", "slot", 1_000)),
        Err(CoreError::RetainedTaskLimitReached { limit: 1 })
    ));
}

#[test]
fn apply_obeys_retained_task_admission_by_name() {
    let config = StateConfig::new().try_with_max_retained_tasks(1).unwrap();
    let state = TaskState::with_epoch(config, "apply-limit".to_string());
    let retained = create(&state, "retained-apply");

    let applied = state
        .apply_desired(&manifest("retained-apply", "changed-slot", 2_000))
        .unwrap();
    assert!(applied.reconcile);
    assert_eq!(applied.task.uid(), retained.uid());

    assert!(matches!(
        state.apply_desired(&manifest("missing-upsert", "slot", 1_000)),
        Err(CoreError::RetainedTaskLimitReached { limit: 1 })
    ));

    let preconditions = WritePreconditions::new()
        .with_resource_version("1")
        .unwrap();
    assert!(matches!(
        state.apply_desired_with_preconditions(
            &manifest("missing-checked-at-limit", "slot", 1_000),
            &preconditions,
        ),
        Err(CoreError::NotFound(_))
    ));
    assert_eq!(state.list_all().len(), 1);
}

#[test]
fn apply_checks_preconditions_before_growth_and_rejects_only_positive_excess() {
    let base = manifest("apply-bytes", "slot", 1_000);
    let grown = annotated_manifest("apply-bytes", 1_024);
    let base_bytes = TaskState::serialized_task_manifest_bytes(&base);
    let grown_bytes = TaskState::serialized_task_manifest_bytes(&grown);
    assert!(grown_bytes > base_bytes);
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(base_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "apply-byte-limit".to_string());
    let stored = state.create_desired(&base).unwrap().task;

    let stale = WritePreconditions::new()
        .with_resource_version("foreign:1")
        .unwrap();
    assert!(matches!(
        state.apply_desired_with_preconditions(&grown, &stale),
        Err(CoreError::Conflict(_))
    ));
    assert!(matches!(
        state.apply_desired(&grown),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded {
            current,
            requested,
            limit,
        }) if current == base_bytes
            && requested == grown_bytes - base_bytes
            && limit == base_bytes
    ));
    assert_eq!(state.get(stored.name()).unwrap(), stored);
    assert_eq!(state.inner.read().retained_task_manifest_bytes, base_bytes);

    let noop = state.apply_desired(&base).unwrap();
    assert!(!noop.reconcile);
    assert_eq!(noop.task, stored);

    let over_limit = TaskState::with_epoch(config, "apply-byte-shrink".to_string());
    over_limit.add_task(grown.clone());
    assert!(over_limit.inner.read().retained_task_manifest_bytes > base_bytes);
    let equal_size_noop = over_limit.apply_desired(&grown).unwrap();
    assert!(!equal_size_noop.reconcile);
    over_limit.apply_desired(&base).unwrap();
    assert_eq!(
        over_limit.inner.read().retained_task_manifest_bytes,
        base_bytes
    );
}

#[test]
fn guarded_missing_apply_precedes_manifest_byte_admission() {
    let retained = manifest("guarded-retained", "slot", 1_000);
    let retained_bytes = TaskState::serialized_task_manifest_bytes(&retained);
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(retained_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "guarded-byte-limit".to_string());
    state.create_desired(&retained).unwrap();
    let preconditions = WritePreconditions::new()
        .with_resource_version("epoch:1")
        .unwrap();

    assert!(matches!(
        state.apply_desired_with_preconditions(
            &annotated_manifest("guarded-missing", 1_024),
            &preconditions,
        ),
        Err(CoreError::NotFound(_))
    ));
}

#[test]
fn delete_and_sweep_release_retained_task_capacity() {
    let config = StateConfig::new().try_with_max_retained_tasks(1).unwrap();
    let deleted_state = TaskState::with_epoch(config, "delete-release".to_string());
    let deleted = create(&deleted_state, "deleted");
    assert!(deleted_state.delete_task(deleted.name()));
    create(&deleted_state, "after-delete");
    assert_eq!(deleted_state.list_all().len(), 1);

    let swept_state = TaskState::with_epoch(config, "sweep-release".to_string());
    let expired = create(&swept_state, "expired");
    let binding = bind(&swept_state, expired.name());
    assert_eq!(
        swept_state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(expired.name().clone())
    );
    let sweep_config = config
        .with_run_ttl(Duration::ZERO)
        .with_task_ttl(Duration::ZERO);
    assert_eq!(swept_state.sweep(&sweep_config), (0, 1));
    create(&swept_state, "after-sweep");
    assert_eq!(swept_state.list_all().len(), 1);
}

#[test]
fn delete_and_sweep_release_exact_manifest_bytes() {
    let deleted = manifest("byte-deleted", "slot", 1_000);
    let after_delete = annotated_manifest("byte-after-delete", 64);
    let delete_limit = TaskState::serialized_task_manifest_bytes(&deleted)
        .max(TaskState::serialized_task_manifest_bytes(&after_delete));
    let delete_config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(delete_limit)
        .unwrap();
    let deleted_state = TaskState::with_epoch(delete_config, "delete-byte-release".to_string());
    let deleted_task = deleted_state.create_desired(&deleted).unwrap().task;
    assert!(matches!(
        deleted_state.create_desired(&after_delete),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded { .. })
    ));
    assert!(deleted_state.delete_task(deleted_task.name()));
    assert_eq!(deleted_state.inner.read().retained_task_manifest_bytes, 0);
    deleted_state.create_desired(&after_delete).unwrap();
    assert_eq!(
        deleted_state.inner.read().retained_task_manifest_bytes,
        TaskState::serialized_task_manifest_bytes(&after_delete)
    );

    let expired = manifest("byte-expired", "slot", 1_000);
    let after_sweep = annotated_manifest("byte-after-sweep", 64);
    let sweep_limit = TaskState::serialized_task_manifest_bytes(&expired)
        .max(TaskState::serialized_task_manifest_bytes(&after_sweep));
    let sweep_config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(sweep_limit)
        .unwrap();
    let swept_state = TaskState::with_epoch(sweep_config, "sweep-byte-release".to_string());
    let expired_task = swept_state.create_desired(&expired).unwrap().task;
    let binding = bind(&swept_state, expired_task.name());
    assert_eq!(
        swept_state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(expired_task.name().clone())
    );
    assert!(matches!(
        swept_state.create_desired(&after_sweep),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded { .. })
    ));
    let immediate = sweep_config
        .with_run_ttl(Duration::ZERO)
        .with_task_ttl(Duration::ZERO);
    assert_eq!(swept_state.sweep(&immediate), (0, 1));
    assert_eq!(swept_state.inner.read().retained_task_manifest_bytes, 0);
    swept_state.create_desired(&after_sweep).unwrap();
    assert_eq!(
        swept_state.inner.read().retained_task_manifest_bytes,
        TaskState::serialized_task_manifest_bytes(&after_sweep)
    );
}

#[test]
fn concurrent_creates_never_exceed_the_retained_task_limit() {
    const LIMIT: usize = 4;
    const ATTEMPTS: usize = 32;

    let config = StateConfig::new()
        .try_with_max_retained_tasks(LIMIT)
        .unwrap();
    let state = TaskState::with_epoch(config, "concurrent-limit".to_string());
    let barrier = Arc::new(std::sync::Barrier::new(ATTEMPTS));
    let results = std::thread::scope(|scope| {
        let handles = (0..ATTEMPTS)
            .map(|index| {
                let state = state.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    state.create_desired(&manifest(&format!("concurrent-{index}"), "slot", 1_000))
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut accepted = 0;
    for result in results {
        match result {
            Ok(_) => accepted += 1,
            Err(CoreError::RetainedTaskLimitReached { limit: LIMIT }) => {}
            Err(error) => panic!("unexpected create result: {error}"),
        }
    }
    assert_eq!(accepted, LIMIT);
    assert_eq!(state.list_all().len(), LIMIT);
}

#[test]
fn concurrent_creates_never_exceed_the_retained_manifest_byte_budget() {
    const ACCEPTED: usize = 4;
    const ATTEMPTS: usize = 32;

    let manifest_bytes =
        TaskState::serialized_task_manifest_bytes(&manifest("concurrent-byte-00", "slot", 1_000));
    let limit = manifest_bytes.checked_mul(ACCEPTED).unwrap();
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(limit)
        .unwrap();
    let state = TaskState::with_epoch(config, "concurrent-byte-limit".to_string());
    let barrier = Arc::new(std::sync::Barrier::new(ATTEMPTS));
    let results = std::thread::scope(|scope| {
        let handles = (0..ATTEMPTS)
            .map(|index| {
                let state = state.clone();
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    let manifest = manifest(&format!("concurrent-byte-{index:02}"), "slot", 1_000);
                    assert_eq!(
                        TaskState::serialized_task_manifest_bytes(&manifest),
                        manifest_bytes
                    );
                    barrier.wait();
                    state.create_desired(&manifest)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut accepted = 0;
    for result in results {
        match result {
            Ok(_) => accepted += 1,
            Err(CoreError::RetainedTaskManifestByteLimitExceeded {
                current,
                requested,
                limit: actual_limit,
            }) => {
                assert_eq!(current, limit);
                assert_eq!(requested, manifest_bytes);
                assert_eq!(actual_limit, limit);
            }
            Err(error) => panic!("unexpected create result: {error}"),
        }
    }
    assert_eq!(accepted, ACCEPTED);
    let inner = state.inner.read();
    assert_eq!(inner.tasks.len(), ACCEPTED);
    assert_eq!(inner.retained_task_manifest_bytes, limit);
    assert_eq!(inner.retained_task_manifest_bytes_by_name.len(), ACCEPTED);
}

#[tokio::test]
async fn rejected_create_has_no_state_watch_or_persistence_side_effects() {
    let recording = Arc::new(RecordingStateSink::default());
    let sink: crate::TaskStateSinkHandle = recording.clone();
    let config = StateConfig::new().try_with_max_retained_tasks(1).unwrap();
    let state = TaskState::with_epoch_and_sink(config, "rejected-create".to_string(), Some(sink));
    create(&state, "accepted");
    let before = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch(&TaskFilter::new(), Some(&before.resource_version))
        .unwrap();
    let history_before = {
        let inner = state.inner.read();
        (inner.watch_history.len(), inner.watch_history_bytes)
    };

    assert!(matches!(
        state.create_desired(&manifest("rejected", "rejected-slot", 1_000)),
        Err(CoreError::RetainedTaskLimitReached { limit: 1 })
    ));

    let after = state.query(&TaskQuery::new()).unwrap();
    assert_eq!(after.resource_version, before.resource_version);
    assert_eq!(after.items, before.items);
    {
        let inner = state.inner.read();
        assert_eq!(inner.ordered_tasks.len(), 1);
        assert!(!inner.by_slot.contains_key("rejected-slot"));
        assert_eq!(
            (inner.watch_history.len(), inner.watch_history_bytes),
            history_before
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), watch.next())
            .await
            .is_err()
    );
    state.shutdown_persistence().await;
    assert_eq!(recording.events.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn byte_rejections_have_no_state_watch_or_persistence_side_effects() {
    let accepted = manifest("byte-side-effects", "slot", 1_000);
    let accepted_bytes = TaskState::serialized_task_manifest_bytes(&accepted);
    let grown = annotated_manifest("byte-side-effects", 1_024);
    let grown_bytes = TaskState::serialized_task_manifest_bytes(&grown);
    let rejected = manifest("byte-side-effects-new", "rejected-byte-slot", 1_000);
    let rejected_bytes = TaskState::serialized_task_manifest_bytes(&rejected);
    let recording = Arc::new(RecordingStateSink::default());
    let sink: crate::TaskStateSinkHandle = recording.clone();
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(accepted_bytes)
        .unwrap();
    let state =
        TaskState::with_epoch_and_sink(config, "rejected-byte-write".to_string(), Some(sink));
    state.create_desired(&accepted).unwrap();
    let before = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch(&TaskFilter::new(), Some(&before.resource_version))
        .unwrap();
    let accounting_before = {
        let inner = state.inner.read();
        (
            inner.watch_history.len(),
            inner.watch_history_bytes,
            inner.retained_task_manifest_bytes,
            inner.retained_task_manifest_bytes_by_name.clone(),
        )
    };

    assert!(matches!(
        state.create_desired(&rejected),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded {
            current,
            requested,
            limit,
        }) if current == accepted_bytes
            && requested == rejected_bytes
            && limit == accepted_bytes
    ));
    assert!(matches!(
        state.apply_desired(&grown),
        Err(CoreError::RetainedTaskManifestByteLimitExceeded {
            current,
            requested,
            limit,
        }) if current == accepted_bytes
            && requested == grown_bytes - accepted_bytes
            && limit == accepted_bytes
    ));

    let after = state.query(&TaskQuery::new()).unwrap();
    assert_eq!(after.resource_version, before.resource_version);
    assert_eq!(after.items, before.items);
    {
        let inner = state.inner.read();
        assert_eq!(inner.ordered_tasks.len(), 1);
        assert!(!inner.by_slot.contains_key("rejected-byte-slot"));
        assert_eq!(
            (
                inner.watch_history.len(),
                inner.watch_history_bytes,
                inner.retained_task_manifest_bytes,
                inner.retained_task_manifest_bytes_by_name.clone(),
            ),
            accounting_before
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(20), watch.next())
            .await
            .is_err()
    );
    state.shutdown_persistence().await;
    assert_eq!(recording.events.lock().unwrap().len(), 1);
}

#[test]
fn test_population_replacement_keeps_manifest_accounting_exact() {
    let larger = annotated_manifest("seeded-byte-accounting", 1_024);
    let smaller = manifest("seeded-byte-accounting", "slot", 1_000);
    let larger_bytes = TaskState::serialized_task_manifest_bytes(&larger);
    let smaller_bytes = TaskState::serialized_task_manifest_bytes(&smaller);
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "seeded-byte-accounting".to_string());

    state.add_task(larger);
    assert_eq!(
        state.inner.read().retained_task_manifest_bytes,
        larger_bytes
    );
    state.add_task(smaller);
    {
        let inner = state.inner.read();
        assert_eq!(inner.retained_task_manifest_bytes, smaller_bytes);
        assert_eq!(inner.retained_task_manifest_bytes_by_name.len(), 1);
    }
    assert!(state.delete_task(&TaskId::new("seeded-byte-accounting").unwrap()));
    let inner = state.inner.read();
    assert_eq!(inner.retained_task_manifest_bytes, 0);
    assert!(inner.retained_task_manifest_bytes_by_name.is_empty());
}

#[test]
fn status_and_run_changes_do_not_change_retained_manifest_bytes() {
    let manifest = manifest("status-byte-accounting", "slot", 1_000);
    let manifest_bytes = TaskState::serialized_task_manifest_bytes(&manifest);
    let config = StateConfig::new()
        .with_max_retained_tasks(None)
        .try_with_max_retained_task_manifest_bytes(manifest_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "status-byte-accounting".to_string());
    let task = state.create_desired(&manifest).unwrap().task;
    let binding = bind(&state, task.name());

    assert!(state.mark_observed(&binding.resource));
    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));

    let inner = state.inner.read();
    assert_eq!(inner.retained_task_manifest_bytes, manifest_bytes);
    assert_eq!(
        inner.retained_task_manifest_bytes_by_name.get(task.name()),
        Some(&manifest_bytes)
    );
    assert_eq!(inner.runs.get(task.name()).map(VecDeque::len), Some(1));
}

#[test]
fn delete_then_create_assigns_a_new_uid() {
    let state = TaskState::new();
    let first = create(&state, "recreated");
    assert!(state.delete_task(first.name()));
    let second = create(&state, "recreated");

    assert_ne!(first.uid(), second.uid());
    assert_eq!(first.name(), second.name());
}

#[test]
fn exact_apply_is_a_true_noop() {
    let state = TaskState::new();
    let first = create(&state, "noop");
    let result = state.apply_desired(&TaskManifest::from(&first)).unwrap();

    assert!(!result.reconcile);
    assert_eq!(result.task, first);
}

#[test]
fn checked_apply_accepts_matching_uid_and_resource_version() {
    let state = TaskState::new();
    let first = create(&state, "checked");
    let preconditions = WritePreconditions::from_task(&first).unwrap();

    let result = state
        .apply_desired_with_preconditions(&TaskManifest::from(&first), &preconditions)
        .unwrap();

    assert!(!result.reconcile);
    assert_eq!(result.task, first);
}

#[test]
fn checked_apply_rejects_every_mismatch_without_consuming_a_version() {
    let state = TaskState::new();
    let first = create(&state, "stale");
    let preconditions = WritePreconditions::new()
        .with_uid(Uid::new("stale-uid").unwrap())
        .with_resource_version("stale-version")
        .unwrap();

    let error = state
        .apply_desired_with_preconditions(&TaskManifest::from(&first), &preconditions)
        .unwrap_err();
    let CoreError::Conflict(conflict) = error else {
        panic!("expected conflict");
    };
    assert_eq!(conflict.name(), first.name());
    assert_eq!(conflict.violations().len(), 2);
    assert_eq!(state.get(first.name()), Some(first.clone()));

    let mut labels = Labels::new();
    labels.insert("changed", "true");
    let changed = TaskManifest::from(&first).with_labels(labels).unwrap();
    let applied = state.apply_desired(&changed).unwrap().task;
    assert_eq!(
        TaskState::parse_resource_version(applied.metadata().resource_version())
            .unwrap()
            .1,
        2
    );
}

#[test]
fn checked_apply_does_not_create_a_missing_resource() {
    let state = TaskState::new();
    let desired = manifest("missing-checked", "slot", 1_000);
    let preconditions = WritePreconditions::new()
        .with_resource_version("1")
        .unwrap();

    let error = state
        .apply_desired_with_preconditions(&desired, &preconditions)
        .unwrap_err();

    assert!(matches!(error, CoreError::NotFound(_)));
    assert!(state.get(desired.name()).is_none());
}

#[test]
fn stale_uid_cannot_update_a_recreated_resource() {
    let state = TaskState::new();
    let first = create(&state, "recreated-checked");
    let stale = WritePreconditions::from_task(&first).unwrap();
    assert!(state.delete_task(first.name()));
    let replacement = create(&state, "recreated-checked");

    let error = state
        .apply_desired_with_preconditions(&TaskManifest::from(&replacement), &stale)
        .unwrap_err();

    assert!(matches!(error, CoreError::Conflict(_)));
    assert_eq!(state.get(replacement.name()), Some(replacement));
}

#[test]
fn metadata_only_apply_changes_only_resource_version_and_metadata() {
    let state = TaskState::new();
    let first = create(&state, "metadata");
    let binding = bind(&state, first.name());
    assert!(state.transition_attempt_starting(&binding, 3));
    let before = state.get(first.name()).unwrap();

    let mut labels = Labels::new();
    labels.insert("team", "platform");
    let desired = manifest("metadata", "slot", 1_000)
        .with_labels(labels.clone())
        .unwrap();
    let result = state.apply_desired(&desired).unwrap();

    assert!(!result.reconcile);
    assert_eq!(result.task.uid(), before.uid());
    assert_eq!(
        result.task.metadata().generation(),
        before.metadata().generation()
    );
    assert_ne!(
        result.task.metadata().resource_version(),
        before.metadata().resource_version()
    );
    assert_eq!(result.task.status(), before.status());
    assert_eq!(result.task.metadata().labels(), &labels);
    assert_eq!(state.binding_for(first.name()), Some(binding));
}

#[test]
fn spec_apply_commits_a_new_pending_generation_without_rollback() {
    let state = TaskState::new();
    let first = create(&state, "changed");
    let old_binding = bind(&state, first.name());
    assert!(state.mark_observed(&old_binding.resource));
    let observed = state.get(first.name()).unwrap();

    let result = state
        .apply_desired(&manifest("changed", "other-slot", 2_000))
        .unwrap();

    assert!(result.reconcile);
    assert_eq!(result.task.uid(), observed.uid());
    assert_eq!(
        result.task.metadata().generation(),
        observed.metadata().generation() + 1
    );
    assert_eq!(
        result.task.status().observed_generation(),
        observed.metadata().generation()
    );
    assert_eq!(result.task.status().phase(), TaskPhase::Pending);
    assert_eq!(result.task.status().attempt(), 0);
    assert_eq!(state.binding_for(first.name()), Some(old_binding));
    assert!(state.list_by_slot("slot").is_empty());
    assert_eq!(state.list_by_slot("other-slot").len(), 1);
}

#[test]
fn apply_missing_creates_a_resource() {
    let state = TaskState::new();
    let result = state
        .apply_desired(&manifest("missing", "slot", 1_000))
        .unwrap();

    assert!(result.reconcile);
    assert!(state.contains_task(result.task.name()));
}

#[test]
fn reconciliation_failure_retains_desired_generation_in_condition() {
    let state = TaskState::new();
    let first = create(&state, "failure");
    let applied = state
        .apply_desired(&manifest("failure", "slot", 2_000))
        .unwrap()
        .task;
    let target = ResourceGeneration::from_task(&applied);

    assert!(state.mark_reconciliation_failed(
        &target,
        "RunnerBuildFailed",
        "runner unavailable".into(),
    ));
    let stored = state.get(applied.name()).unwrap();
    assert_eq!(stored.spec(), applied.spec());
    assert_eq!(stored.metadata().generation(), target.generation);
    assert_eq!(stored.status().observed_generation(), target.generation);
    assert_eq!(stored.status().phase(), TaskPhase::Pending);
    assert_eq!(stored.status().attempt(), 0);
    assert!(stored.status().error().is_none());
    assert_eq!(
        stored.status().reconciled().status(),
        ConditionStatus::False
    );
    assert_eq!(stored.status().reconciled().reason(), "RunnerBuildFailed");
    assert_eq!(stored.status().reconciled().message(), "runner unavailable");
    assert_eq!(stored.uid(), first.uid());
}

#[test]
fn taskvisor_intake_pending_ensure_is_exact_and_refuses_accepted_state() {
    let state = TaskState::new();
    let task = create(&state, "intake-pending");
    let target = ResourceGeneration::from_task(&task);

    let admission = state
        .admit_state_write_blocking(StateMutationEventCapacity::TaskChange)
        .unwrap();
    assert!(state.ensure_taskvisor_intake_pending_admitted(&target, admission));
    let pending = state.get(task.name()).unwrap();
    assert_eq!(
        pending.status().reconciled().reason(),
        TASKVISOR_INTAKE_PENDING_REASON
    );
    let pending_version = pending.metadata().resource_version().to_owned();

    let admission = state
        .admit_state_write_blocking(StateMutationEventCapacity::TaskChange)
        .unwrap();
    assert!(state.ensure_taskvisor_intake_pending_admitted(&target, admission));
    assert_eq!(
        state
            .get(task.name())
            .unwrap()
            .metadata()
            .resource_version(),
        pending_version,
        "an exact condition must not consume another resource version"
    );

    assert!(state.mark_observed(&target));
    let accepted = state.get(task.name()).unwrap();
    let accepted_version = accepted.metadata().resource_version().to_owned();
    let admission = state
        .admit_state_write_blocking(StateMutationEventCapacity::TaskChange)
        .unwrap();
    assert!(!state.ensure_taskvisor_intake_pending_admitted(&target, admission));
    let retained = state.get(task.name()).unwrap();
    assert_eq!(
        retained.status().reconciled().status(),
        ConditionStatus::True
    );
    assert_eq!(retained.status().reconciled().reason(), "RuntimeAccepted");
    assert_eq!(
        retained.metadata().resource_version(),
        accepted_version,
        "a refused transition must not consume a resource version"
    );
}

#[test]
fn identical_apply_reschedules_only_a_failed_reconciliation() {
    let state = TaskState::new();
    let task = create(&state, "retry");
    let target = ResourceGeneration::from_task(&task);
    assert!(state.mark_reconciliation_failed(
        &target,
        "RunnerBuildFailed",
        "runner unavailable".into(),
    ));

    let retry = state.apply_desired(&TaskManifest::from(&task)).unwrap();
    assert!(retry.reconcile);
    assert_eq!(retry.task.metadata().generation(), target.generation);
    assert_eq!(
        retry.task.status().reconciled().status(),
        ConditionStatus::Unknown
    );

    let duplicate = state.apply_desired(&TaskManifest::from(&task)).unwrap();
    assert!(!duplicate.reconcile);
}

#[test]
fn authoritative_attempt_and_generation_are_recorded_in_status_and_run() {
    let state = TaskState::new();
    let task = create(&state, "attempt");
    let binding = bind(&state, task.name());

    assert!(state.transition_attempt_starting(&binding, 4));
    assert!(state.transition_attempt_finished(
        &binding,
        4,
        TaskPhase::Failed,
        Some("exit".into()),
        Some(17),
    ));

    let stored = state.get(task.name()).unwrap();
    assert_eq!(stored.status().attempt(), 4);
    assert_eq!(
        stored.status().observed_generation(),
        binding.resource.generation
    );
    let runs = state.list_runs(task.name());
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].generation(), binding.resource.generation);
    assert_eq!(runs[0].attempt(), 4);
    assert_eq!(runs[0].exit_code(), Some(17));
}

#[test]
fn each_run_snapshots_the_workload_gvk_of_its_generation() {
    let state = TaskState::new();
    let first = create(&state, "workload-history");
    let old_binding = bind(&state, first.name());
    assert!(state.transition_attempt_finished(
        &old_binding,
        1,
        TaskPhase::Succeeded,
        None,
        Some(0),
    ));

    let routed_workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "true".into(),
            args: vec![],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ));
    let desired = TaskManifest::new(
        first.name().clone(),
        TaskSpec::builder("slot", routed_workload, 1_000_u64)
            .build()
            .unwrap(),
    )
    .unwrap();
    let applied = state.apply_desired(&desired).unwrap().task;
    let new_binding = bind(&state, applied.name());
    assert!(state.transition_attempt_starting(&new_binding, 1));

    let runs = state.list_runs(applied.name());
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].generation(), 1);
    assert_eq!(runs[0].workload().api_version(), "solti.io/v1");
    assert_eq!(runs[0].workload().kind(), "Embedded");
    assert_eq!(runs[1].generation(), 2);
    assert_eq!(runs[1].workload().api_version(), "solti.io/v1");
    assert_eq!(runs[1].workload().kind(), "Subprocess");
}

#[test]
fn duplicate_terminal_attempt_is_an_exact_noop() {
    let state = TaskState::new();
    let task = create(&state, "duplicate-finish");
    let binding = bind(&state, task.name());

    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));

    let before_task = state.get(task.name()).unwrap();
    let before_runs = state.list_runs(task.name());
    let terminal_marker = SystemTime::UNIX_EPOCH + Duration::from_secs(17);
    state
        .inner
        .write()
        .terminal_since
        .insert(task.name().clone(), terminal_marker);

    assert!(!state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));

    assert_eq!(state.get(task.name()).unwrap(), before_task);
    assert_eq!(state.list_runs(task.name()), before_runs);
    assert_eq!(
        state.inner.read().terminal_since.get(task.name()),
        Some(&terminal_marker)
    );
}

#[test]
fn duplicate_terminal_attempt_stays_a_noop_after_run_eviction() {
    let state = TaskState::new();
    state.set_max_runs_per_task(0);
    let task = create(&state, "duplicate-evicted-finish");
    let binding = bind(&state, task.name());

    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));
    assert!(state.list_runs(task.name()).is_empty());

    let before_task = state.get(task.name()).unwrap();
    assert!(!state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));
    assert_eq!(state.get(task.name()).unwrap(), before_task);
    assert!(state.list_runs(task.name()).is_empty());
}

#[test]
fn terminal_event_without_start_creates_the_exact_authoritative_run() {
    let state = TaskState::new();
    let task = create(&state, "lost-start");
    let binding = bind(&state, task.name());

    assert!(state.transition_attempt_finished(&binding, 5, TaskPhase::Succeeded, None, Some(0),));

    let stored = state.get(task.name()).unwrap();
    assert_eq!(stored.status().attempt(), 5);
    let runs = state.list_runs(task.name());
    assert_eq!(runs.len(), 1);
    assert_eq!((runs[0].generation(), runs[0].attempt()), (1, 5));
}

#[test]
fn later_terminal_attempt_closes_an_unresolved_earlier_run() {
    let state = TaskState::new();
    let task = create(&state, "lost-terminal");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));

    assert!(state.transition_attempt_finished(&binding, 2, TaskPhase::Succeeded, None, None,));

    let runs = state.list_runs(task.name());
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].attempt(), 1);
    assert_eq!(runs[0].phase(), TaskPhase::Failed);
    assert!(
        runs[0]
            .error()
            .is_some_and(|error| error.contains("later attempt finished"))
    );
    assert_eq!(runs[1].attempt(), 2);
    assert_eq!(runs[1].phase(), TaskPhase::Succeeded);
}

#[test]
fn authoritative_terminals_without_start_remain_bounded() {
    let state = TaskState::new();
    state.set_max_runs_per_task(2);
    let task = create(&state, "bounded-terminal");
    let binding = bind(&state, task.name());
    for attempt in 1..=3 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            Some(format!("attempt {attempt}")),
            None,
        ));
    }

    let runs = state.list_runs(task.name());
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].attempt(), 2);
    assert_eq!(runs[1].attempt(), 3);
}

#[test]
fn run_pages_keep_an_exact_stable_snapshot_prefix() {
    let state = TaskState::with_epoch(StateConfig::new(), "task-epoch".to_string());
    let task = create(&state, "run-pages");
    let binding = bind(&state, task.name());
    for attempt in 1..=3 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            Some(format!("attempt {attempt}")),
            None,
        ));
    }

    let query = TaskRunQuery::new().with_limit(2);
    let first = state.query_runs(task.name(), &query).unwrap().unwrap();
    assert_eq!(
        first.items.iter().map(TaskRun::attempt).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(first.task, *task.name());
    assert_eq!(first.task_uid, *task.uid());
    assert_eq!(first.resource_version, "runs-task-epoch:3");
    assert_eq!(first.remaining_item_count, 1);
    let continuation = first.continuation.unwrap();

    assert!(state.transition_attempt_finished(&binding, 4, TaskPhase::Succeeded, None, Some(0),));
    let second = state
        .query_runs(task.name(), &query.with_continuation(continuation))
        .unwrap()
        .unwrap();
    assert_eq!(
        second
            .items
            .iter()
            .map(TaskRun::attempt)
            .collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(second.remaining_item_count, 0);
    assert!(second.continuation.is_none());

    let fresh = state
        .query_runs(task.name(), &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(fresh.items.len(), 4);
}

#[test]
fn run_revision_rolls_to_a_new_epoch_after_counter_exhaustion() {
    let state = TaskState::with_epoch(StateConfig::new(), "roll".to_string());
    let task = create(&state, "run-rollover");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));
    let old_cursor =
        TaskRunContinuation::new("runs-roll:1", task.name().clone(), task.uid().clone(), 1, 1)
            .unwrap();

    state
        .write(StateMutationEventCapacity::None)
        .run_resource_version = u64::MAX;
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));

    let page = state
        .query_runs(task.name(), &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(page.resource_version, "next-runs-roll:1");
    let inner = state.inner.read();
    assert_eq!(inner.run_history.len(), 1);
    assert_eq!(inner.run_history.front().unwrap().revision, 1);
    assert_eq!(inner.run_compacted_through, 0);
    drop(inner);
    assert!(matches!(
        state.query_runs(
            task.name(),
            &TaskRunQuery::new().with_continuation(old_cursor)
        ),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
}

#[test]
fn run_continuation_preserves_an_active_run_after_live_finish() {
    let state = TaskState::with_epoch(StateConfig::new(), "frozen-run".to_string());
    let task = create(&state, "frozen-active-run");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, Some(0),));
    assert!(state.transition_attempt_starting(&binding, 2));

    let query = TaskRunQuery::new().with_limit(1);
    let first = state.query_runs(task.name(), &query).unwrap().unwrap();
    assert_eq!(first.items[0].attempt(), 1);
    let continuation = first.continuation.unwrap();

    assert!(state.transition_attempt_finished(&binding, 2, TaskPhase::Succeeded, None, Some(0),));

    let frozen = state
        .query_runs(task.name(), &query.with_continuation(continuation))
        .unwrap()
        .unwrap();
    assert_eq!(frozen.items.len(), 1);
    assert_eq!(frozen.items[0].attempt(), 2);
    assert_eq!(frozen.items[0].phase(), TaskPhase::Running);
    assert!(frozen.items[0].is_active());
    assert!(frozen.items[0].finished_at().is_none());

    let current = state
        .query_runs(task.name(), &TaskRunQuery::new())
        .unwrap()
        .unwrap();
    assert_eq!(current.items[1].phase(), TaskPhase::Succeeded);
    assert!(!current.items[1].is_active());
}

#[test]
fn run_item_byte_limit_keeps_complete_items_and_a_contiguous_prefix() {
    let state = TaskState::new();
    let task = create(&state, "run-item-bytes");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_finished(
        &binding,
        1,
        TaskPhase::Failed,
        Some("x".repeat(2_048)),
        None,
    ));
    assert!(state.transition_attempt_finished(&binding, 2, TaskPhase::Succeeded, None, Some(0),));
    let runs = state.list_runs(task.name());
    let first_bytes = TaskState::serialized_run_payload_bytes(&runs[0]);
    let query = TaskRunQuery::new()
        .with_item_byte_limit(std::num::NonZeroUsize::new(first_bytes - 1).unwrap());

    let first = state.query_runs(task.name(), &query).unwrap().unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].attempt(), 1);
    assert_eq!(first.remaining_item_count, 1);

    let second = state
        .query_runs(
            task.name(),
            &query.with_continuation(first.continuation.unwrap()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].attempt(), 2);
    assert_eq!(second.remaining_item_count, 0);
}

#[test]
fn run_continuation_survives_delete_and_recreate_while_journal_is_retained() {
    let state = TaskState::with_epoch(StateConfig::new(), "recreate-epoch".to_string());
    let original = create(&state, "run-recreate");
    let original_uid = original.uid().clone();
    let binding = bind(&state, original.name());
    for attempt in 1..=3 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            None,
            None,
        ));
    }
    let query = TaskRunQuery::new().with_limit(1);
    let first = state.query_runs(original.name(), &query).unwrap().unwrap();
    assert_eq!(first.items[0].attempt(), 1);
    let continuation = first.continuation.unwrap();

    assert!(state.delete_task(original.name()));
    let replacement = create(&state, "run-recreate");
    assert_ne!(replacement.uid(), &original_uid);
    let replacement_binding = bind(&state, replacement.name());
    assert!(state.transition_attempt_finished(
        &replacement_binding,
        1,
        TaskPhase::Succeeded,
        None,
        Some(0),
    ));

    let second = state
        .query_runs(
            replacement.name(),
            &query.clone().with_continuation(continuation),
        )
        .unwrap()
        .unwrap();
    assert_eq!(second.task_uid, original_uid);
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].attempt(), 2);
    assert_eq!(second.remaining_item_count, 1);
    let third = state
        .query_runs(
            replacement.name(),
            &query.with_continuation(second.continuation.unwrap()),
        )
        .unwrap()
        .unwrap();
    assert_eq!(third.task_uid, original_uid);
    assert_eq!(third.items.len(), 1);
    assert_eq!(third.items[0].attempt(), 3);
    assert_eq!(third.remaining_item_count, 0);
    assert!(third.continuation.is_none());
}

#[test]
fn test_fixture_replacement_removes_runs_under_the_original_uid() {
    let state = TaskState::with_epoch(StateConfig::new(), "fixture-epoch".to_string());
    let original = create(&state, "run-fixture-replace");
    let original_uid = original.uid().clone();
    let binding = bind(&state, original.name());
    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            None,
            None,
        ));
    }
    let query = TaskRunQuery::new().with_limit(1);
    let continuation = state
        .query_runs(original.name(), &query)
        .unwrap()
        .unwrap()
        .continuation
        .unwrap();

    state.add_task(manifest("run-fixture-replace", "slot", 1_000));
    let replacement = state.get(original.name()).unwrap();
    assert_ne!(replacement.uid(), &original_uid);
    assert!(state.list_runs(original.name()).is_empty());

    let page = state
        .query_runs(original.name(), &query.with_continuation(continuation))
        .unwrap()
        .unwrap();
    assert_eq!(page.task_uid, original_uid);
    assert_eq!(page.items[0].attempt(), 2);
}

#[test]
fn cap_eviction_is_reversible_for_run_continuations() {
    let state = TaskState::with_epoch(StateConfig::new(), "cap-epoch".to_string());
    state.set_max_runs_per_task(2);
    let task = create(&state, "run-cap-snapshot");
    let binding = bind(&state, task.name());
    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            None,
            None,
        ));
    }
    let query = TaskRunQuery::new().with_limit(1);
    let first = state.query_runs(task.name(), &query).unwrap().unwrap();
    let continuation = first.continuation.unwrap();

    assert!(state.transition_attempt_finished(&binding, 3, TaskPhase::Failed, None, None,));
    assert_eq!(state.list_runs(task.name())[0].attempt(), 2);

    let second = state
        .query_runs(task.name(), &query.with_continuation(continuation))
        .unwrap()
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].attempt(), 2);
    assert_eq!(second.remaining_item_count, 0);
}

#[test]
fn sweep_removal_is_reversible_for_run_continuations() {
    let state = TaskState::with_epoch(StateConfig::new(), "sweep-epoch".to_string());
    let task = create(&state, "run-sweep-snapshot");
    let binding = bind(&state, task.name());
    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));
    }
    let query = TaskRunQuery::new().with_limit(1);
    let first = state.query_runs(task.name(), &query).unwrap().unwrap();
    let continuation = first.continuation.unwrap();

    let config = StateConfig::new().with_run_ttl(Duration::ZERO);
    assert_eq!(state.sweep(&config).0, 2);
    assert!(state.list_runs(task.name()).is_empty());

    let second = state
        .query_runs(task.name(), &query.with_continuation(continuation))
        .unwrap()
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].attempt(), 2);
}

#[test]
fn active_closure_and_next_attempt_share_one_reversible_run_revision() {
    let state = TaskState::with_epoch(StateConfig::new(), "batch-epoch".to_string());
    let task = create(&state, "run-batch");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_starting(&binding, 2));

    let inner = state.inner.read();
    assert_eq!(inner.run_resource_version, 2);
    let batch = inner.run_history.back().unwrap();
    assert_eq!(batch.revision, 2);
    assert_eq!(batch.changes.len(), 2);
    let snapshot = TaskState::run_snapshot_at_resource_version(
        &inner,
        "runs-batch-epoch:1",
        task.name(),
        task.uid(),
    )
    .unwrap();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.get(&(1, 1)).unwrap().is_active());
}

#[test]
fn run_journal_compaction_expires_old_continuations() {
    let config = StateConfig::new().try_with_run_history_capacity(1).unwrap();
    let state = TaskState::with_epoch(config, "compact-epoch".to_string());
    let task = create(&state, "run-compaction");
    let binding = bind(&state, task.name());
    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            None,
            None,
        ));
    }
    let query = TaskRunQuery::new().with_limit(1);
    let continuation = state
        .query_runs(task.name(), &query)
        .unwrap()
        .unwrap()
        .continuation
        .unwrap();

    assert!(state.transition_attempt_finished(&binding, 3, TaskPhase::Failed, None, None,));
    assert!(
        state
            .query_runs(
                task.name(),
                &query.clone().with_continuation(continuation.clone()),
            )
            .is_ok()
    );

    assert!(state.transition_attempt_finished(&binding, 4, TaskPhase::Failed, None, None,));
    assert!(matches!(
        state.query_runs(task.name(), &query.with_continuation(continuation)),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
}

#[test]
fn run_journal_retains_a_batch_at_the_exact_byte_budget() {
    let change = run_journal_insertion("run-a", 1);
    let serialized_bytes = TaskState::serialized_run_change_bytes(std::slice::from_ref(&change));
    let config = StateConfig::new()
        .try_with_run_history_byte_budget(serialized_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "exact-run-bytes".to_string());

    let mut inner = state.write(StateMutationEventCapacity::None);
    TaskState::record_run_snapshot_changes(&mut inner, vec![change]);

    assert_eq!(inner.run_history.len(), 1);
    assert_eq!(inner.run_history_bytes, serialized_bytes);
    assert_eq!(
        inner.run_history.front().unwrap().serialized_bytes,
        serialized_bytes
    );
    assert_eq!(inner.run_compacted_through, 0);
    assert_eq!(inner.run_resource_version, 1);
}

#[test]
fn oversized_run_batch_compacts_history_and_keeps_the_current_revision_valid() {
    let change = run_journal_insertion("run-a", 1);
    let serialized_bytes = TaskState::serialized_run_change_bytes(std::slice::from_ref(&change));
    let config = StateConfig::new()
        .try_with_run_history_byte_budget(serialized_bytes - 1)
        .unwrap();
    let state = TaskState::with_epoch(config, "oversized-run-bytes".to_string());

    let mut inner = state.write(StateMutationEventCapacity::None);
    TaskState::record_run_snapshot_changes(&mut inner, vec![change]);

    assert!(inner.run_history.is_empty());
    assert_eq!(inner.run_history_bytes, 0);
    assert_eq!(inner.run_compacted_through, 1);
    assert!(
        TaskState::run_snapshot_at_resource_version(
            &inner,
            "runs-oversized-run-bytes:1",
            &TaskId::new("run-a").unwrap(),
            &Uid::new("run-journal-test-uid").unwrap(),
        )
        .is_ok()
    );
    assert!(matches!(
        TaskState::run_snapshot_at_resource_version(
            &inner,
            "runs-oversized-run-bytes:0",
            &TaskId::new("run-a").unwrap(),
            &Uid::new("run-journal-test-uid").unwrap(),
        ),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
}

#[test]
fn run_journal_byte_budget_can_evict_multiple_batches() {
    let first = run_journal_insertion("run-a", 1);
    let second = run_journal_insertion("run-b", 2);
    let third = run_journal_insertion("run-c", 3);
    let batch_bytes = TaskState::serialized_run_change_bytes(std::slice::from_ref(&first));
    assert_eq!(
        TaskState::serialized_run_change_bytes(std::slice::from_ref(&second)),
        batch_bytes
    );
    assert_eq!(
        TaskState::serialized_run_change_bytes(std::slice::from_ref(&third)),
        batch_bytes
    );
    let config = StateConfig::new()
        .try_with_run_history_byte_budget(batch_bytes.checked_mul(2).unwrap())
        .unwrap();
    let state = TaskState::with_epoch(config, "evict-run-bytes".to_string());

    let mut inner = state.write(StateMutationEventCapacity::None);
    TaskState::record_run_snapshot_changes(&mut inner, vec![first]);
    TaskState::record_run_snapshot_changes(&mut inner, vec![second]);
    TaskState::record_run_snapshot_changes(&mut inner, vec![third]);

    assert_eq!(inner.run_history.len(), 2);
    assert_eq!(inner.run_history_bytes, batch_bytes * 2);
    assert_eq!(inner.run_history.front().unwrap().revision, 2);
    assert_eq!(inner.run_history.back().unwrap().revision, 3);
    assert_eq!(inner.run_compacted_through, 1);
    assert!(
        TaskState::run_snapshot_at_resource_version(
            &inner,
            "runs-evict-run-bytes:1",
            &TaskId::new("run-a").unwrap(),
            &Uid::new("run-journal-test-uid").unwrap(),
        )
        .is_ok()
    );
}

#[test]
fn task_resource_versions_are_foreign_to_run_snapshots() {
    let state = TaskState::with_epoch(StateConfig::new(), "foreign-epoch".to_string());
    let task = create(&state, "run-foreign-version");
    let binding = bind(&state, task.name());
    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            None,
            None,
        ));
    }
    let cursor = TaskRunContinuation::new(
        state
            .get(task.name())
            .unwrap()
            .metadata()
            .resource_version(),
        task.name().clone(),
        task.uid().clone(),
        1,
        1,
    )
    .unwrap();

    assert!(matches!(
        state.query_runs(task.name(), &TaskRunQuery::new().with_continuation(cursor),),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
}

#[test]
fn run_predicate_filters_before_pagination() {
    let state = TaskState::new();
    let task = create(&state, "run-filter");
    let binding = bind(&state, task.name());
    for attempt in 1..=4 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Failed,
            None,
            None,
        ));
    }
    let query = TaskRunQuery::new().with_limit(1);

    let first = state
        .query_runs_where(task.name(), &query, |run| run.attempt() % 2 == 1)
        .unwrap()
        .unwrap();
    assert_eq!(first.items[0].attempt(), 1);
    assert_eq!(first.remaining_item_count, 1);
    let second = state
        .query_runs_where(
            task.name(),
            &query.with_continuation(first.continuation.unwrap()),
            |run| run.attempt() % 2 == 1,
        )
        .unwrap()
        .unwrap();
    assert_eq!(second.items[0].attempt(), 3);
    assert_eq!(second.remaining_item_count, 0);
}

#[test]
fn run_visibility_predicates_run_without_the_state_lock() {
    let state = TaskState::new();
    let task = create(&state, "run-visibility-lock");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));

    let page = state
        .query_runs_where_visible(
            task.name(),
            &TaskRunQuery::new(),
            |_| {
                assert!(state.inner.try_write().is_some());
                true
            },
            |_| {
                assert!(state.inner.try_write().is_some());
                true
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(page.items.len(), 1);
}

#[test]
fn zero_run_cap_keeps_only_active_runs() {
    let state = TaskState::new();
    state.set_max_runs_per_task(0);
    let task = create(&state, "no-history");
    let binding = bind(&state, task.name());

    assert!(state.transition_attempt_starting(&binding, 1));
    assert_eq!(state.list_runs(task.name()).len(), 1);

    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, None,));
    assert!(state.list_runs(task.name()).is_empty());
}

#[test]
fn completed_run_cap_does_not_count_an_active_run() {
    let state = TaskState::new();
    state.set_max_runs_per_task(2);
    let task = create(&state, "completed-history-cap");
    let binding = bind(&state, task.name());

    for attempt in 1..=2 {
        assert!(state.transition_attempt_finished(
            &binding,
            attempt,
            TaskPhase::Succeeded,
            None,
            Some(0),
        ));
    }
    assert!(state.transition_attempt_starting(&binding, 3));
    let runs = state.list_runs(task.name());
    assert_eq!(
        runs.iter().map(TaskRun::attempt).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(runs[2].is_active());

    assert!(state.transition_attempt_finished(&binding, 3, TaskPhase::Succeeded, None, Some(0),));
    assert_eq!(
        state
            .list_runs(task.name())
            .iter()
            .map(TaskRun::attempt)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
}

#[test]
fn finalizer_enforces_the_completed_run_cap_in_one_revision() {
    let state = TaskState::new();
    state.set_max_runs_per_task(0);
    let task = create(&state, "finalizer-run-cap");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));

    assert_eq!(
        state.finalize_if_bound(binding.tv.get(), TaskPhase::Succeeded, None, Some(0), false,),
        Some(task.name().clone())
    );

    assert!(state.list_runs(task.name()).is_empty());
    let inner = state.inner.read();
    assert_eq!(inner.run_resource_version, 2);
    assert_eq!(inner.run_history.back().unwrap().changes.len(), 2);
}

#[test]
fn finalizer_finish_and_cap_eviction_are_reversible_for_a_continuation() {
    let state = TaskState::new();
    state.set_max_runs_per_task(1);
    let task = create(&state, "finalizer-snapshot-cap");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Failed, None, Some(1),));
    assert!(state.transition_attempt_starting(&binding, 2));
    let query = TaskRunQuery::new().with_limit(1);
    let first = state.query_runs(task.name(), &query).unwrap().unwrap();
    assert_eq!(first.items[0].attempt(), 1);
    let continuation = first.continuation.unwrap();

    assert_eq!(
        state.finalize_if_bound(binding.tv.get(), TaskPhase::Succeeded, None, Some(0), false,),
        Some(task.name().clone())
    );
    assert_eq!(state.list_runs(task.name())[0].attempt(), 2);

    let frozen = state
        .query_runs(task.name(), &query.with_continuation(continuation))
        .unwrap()
        .unwrap();
    assert_eq!(frozen.items.len(), 1);
    assert_eq!(frozen.items[0].attempt(), 2);
    assert_eq!(frozen.items[0].phase(), TaskPhase::Running);
    assert_eq!(frozen.remaining_item_count, 0);
}

#[test]
fn old_generation_event_can_close_its_run_but_cannot_mutate_current_status() {
    let state = TaskState::new();
    let first = create(&state, "generation-fence");
    let old = bind(&state, first.name());
    assert!(state.transition_attempt_starting(&old, 1));

    let current = state
        .apply_desired(&manifest("generation-fence", "slot", 2_000))
        .unwrap()
        .task;
    assert_eq!(current.status().phase(), TaskPhase::Pending);

    assert!(state.transition_attempt_finished(
        &old,
        1,
        TaskPhase::Failed,
        Some("late".into()),
        Some(1),
    ));

    let stored = state.get(first.name()).unwrap();
    assert_eq!(stored.metadata().generation(), 2);
    assert_eq!(stored.status().phase(), TaskPhase::Pending);
    assert_eq!(stored.status().attempt(), 0);
    let old_run = &state.list_runs(first.name())[0];
    assert_eq!(old_run.generation(), 1);
    assert_eq!(old_run.phase(), TaskPhase::Failed);

    let before_task = stored;
    let before_runs = state.list_runs(first.name());
    assert!(!state.transition_attempt_finished(
        &old,
        1,
        TaskPhase::Failed,
        Some("late".into()),
        Some(1),
    ));
    assert_eq!(state.get(first.name()).unwrap(), before_task);
    assert_eq!(state.list_runs(first.name()), before_runs);
}

#[test]
fn stale_uid_cannot_mutate_a_recreated_resource() {
    let state = TaskState::new();
    let first = create(&state, "uid-fence");
    let stale = bind(&state, first.name());
    assert!(state.delete_task(first.name()));
    let replacement = create(&state, "uid-fence");

    assert!(!state.transition_attempt_finished(
        &stale,
        1,
        TaskPhase::Failed,
        Some("stale".into()),
        None,
    ));
    let stored = state.get(replacement.name()).unwrap();
    assert_eq!(stored.uid(), replacement.uid());
    assert_eq!(stored.status().phase(), TaskPhase::Pending);
    assert!(state.list_runs(replacement.name()).is_empty());
}

#[test]
fn lower_attempt_cannot_regress_current_status() {
    let state = TaskState::new();
    let task = create(&state, "attempt-fence");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 3));

    assert!(state.transition_attempt_finished(
        &binding,
        2,
        TaskPhase::Failed,
        Some("late attempt".into()),
        None,
    ));

    let stored = state.get(task.name()).unwrap();
    assert_eq!(stored.status().phase(), TaskPhase::Running);
    assert_eq!(stored.status().attempt(), 3);
    assert_eq!(
        state
            .list_runs(task.name())
            .iter()
            .find(|run| run.attempt() == 2)
            .unwrap()
            .phase(),
        TaskPhase::Failed
    );
}

#[test]
fn late_or_duplicate_start_cannot_reopen_attempt_history() {
    let state = TaskState::new();
    let task = create(&state, "start-fence");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, None,));

    assert!(!state.transition_attempt_starting(&binding, 1));
    assert!(state.transition_attempt_starting(&binding, 3));
    assert!(!state.transition_attempt_starting(&binding, 2));

    let stored = state.get(task.name()).unwrap();
    assert_eq!(stored.status().phase(), TaskPhase::Running);
    assert_eq!(stored.status().attempt(), 3);
    let runs = state.list_runs(task.name());
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].phase(), TaskPhase::Succeeded);
    assert_eq!(runs[1].attempt(), 3);
}

#[test]
fn every_embedded_resource_name_and_slot_is_public() {
    let state = TaskState::new();
    create(&state, "embedded-public");
    state
        .create_desired(&manifest("solti-state-sweep", "solti-state-sweep", 1_000))
        .unwrap();
    state
        .create_desired(&manifest("user-in-sweep-slot", "solti-state-sweep", 1_000))
        .unwrap();

    assert_eq!(state.list_all().len(), 3);
    assert_eq!(state.query(&TaskQuery::new()).unwrap().items.len(), 3);
    assert_eq!(state.list_by_slot("slot").len(), 1);
    assert_eq!(state.list_by_slot("solti-state-sweep").len(), 2);
    assert!(
        state
            .get(&TaskId::new("solti-state-sweep").unwrap())
            .is_some()
    );
}

#[test]
fn adapter_predicate_runs_before_pagination() {
    let state = TaskState::new();
    create(&state, "a-visible");
    create(&state, "b-hidden");
    create(&state, "c-visible");
    let query = TaskQuery::new().with_limit(1);

    let first = state
        .query_where(&query, |task| task.name().as_str() != "b-hidden")
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].name().as_str(), "a-visible");
    assert_eq!(first.remaining_item_count, 1);

    let second = state
        .query_where(
            &query.with_continuation(first.continuation.unwrap()),
            |task| task.name().as_str() != "b-hidden",
        )
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].name().as_str(), "c-visible");
    assert_eq!(second.remaining_item_count, 0);
    assert!(second.continuation.is_none());
}

#[test]
fn item_byte_limit_stops_before_the_first_non_fitting_task() {
    let state = TaskState::new();
    let first = create(&state, "a-first");
    let second = create(&state, "b-second");
    let first_bytes = TaskState::serialized_task_payload_bytes(None, Some(&first));
    let second_bytes = TaskState::serialized_task_payload_bytes(None, Some(&second));
    let exact_two = first_bytes
        .checked_add(1)
        .and_then(|bytes| bytes.checked_add(second_bytes))
        .unwrap();

    let short =
        TaskQuery::new().with_item_byte_limit(std::num::NonZeroUsize::new(exact_two - 1).unwrap());
    let first_page = state.query(&short).unwrap();
    assert_eq!(first_page.items, vec![first]);
    assert_eq!(first_page.remaining_item_count, 1);

    let second_page = state
        .query(&short.with_continuation(first_page.continuation.unwrap()))
        .unwrap();
    assert_eq!(second_page.items, vec![second]);
    assert_eq!(second_page.remaining_item_count, 0);

    let exact =
        TaskQuery::new().with_item_byte_limit(std::num::NonZeroUsize::new(exact_two).unwrap());
    assert_eq!(state.query(&exact).unwrap().items.len(), 2);
}

#[test]
fn item_byte_limit_keeps_a_contiguous_prefix() {
    let state = TaskState::new();
    let first = create(&state, "a-first");
    let mut annotations = Annotations::new();
    annotations.insert("example.io/payload", "x".repeat(2_048));
    let middle = state
        .create_desired(
            &manifest("b-middle", "slot", 1_000)
                .with_annotations(annotations)
                .unwrap(),
        )
        .unwrap()
        .task;
    let last = create(&state, "c-last");
    let first_bytes = TaskState::serialized_task_payload_bytes(None, Some(&first));
    let middle_bytes = TaskState::serialized_task_payload_bytes(None, Some(&middle));
    let last_bytes = TaskState::serialized_task_payload_bytes(None, Some(&last));
    assert!(first_bytes + 1 + last_bytes <= middle_bytes);
    let query =
        TaskQuery::new().with_item_byte_limit(std::num::NonZeroUsize::new(middle_bytes).unwrap());

    let page = state.query(&query).unwrap();

    assert_eq!(page.items, vec![first]);
    assert_eq!(page.remaining_item_count, 2);
    assert_eq!(
        page.continuation.as_ref().unwrap().after().as_str(),
        "a-first"
    );
}

#[test]
fn predicate_filters_before_item_byte_accounting() {
    let state = TaskState::new();
    let mut annotations = Annotations::new();
    annotations.insert("example.io/payload", "x".repeat(2_048));
    let hidden = state
        .create_desired(
            &manifest("a-hidden", "slot", 1_000)
                .with_annotations(annotations)
                .unwrap(),
        )
        .unwrap()
        .task;
    let visible = create(&state, "b-visible");
    let hidden_bytes = TaskState::serialized_task_payload_bytes(None, Some(&hidden));
    let visible_bytes = TaskState::serialized_task_payload_bytes(None, Some(&visible));
    assert!(hidden_bytes > visible_bytes);
    let query =
        TaskQuery::new().with_item_byte_limit(std::num::NonZeroUsize::new(visible_bytes).unwrap());

    let page = state
        .query_where(&query, |task| task.name().as_str() != "a-hidden")
        .unwrap();

    assert_eq!(page.items, vec![visible]);
    assert_eq!(page.remaining_item_count, 0);
    assert!(page.continuation.is_none());
}

#[test]
fn oversized_first_item_is_returned_for_transport_measurement() {
    let state = TaskState::new();
    let task = create(&state, "oversized");
    let next = create(&state, "oversized-next");
    let task_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let query =
        TaskQuery::new().with_item_byte_limit(std::num::NonZeroUsize::new(task_bytes - 1).unwrap());

    let page = state.query(&query).unwrap();

    assert_eq!(page.items, vec![task]);
    assert_eq!(page.remaining_item_count, 1);
    assert_eq!(
        page.continuation.as_ref().unwrap().after().as_str(),
        "oversized"
    );
    let next_page = state
        .query(&query.with_continuation(page.continuation.unwrap()))
        .unwrap();
    assert_eq!(next_page.items, vec![next]);
}

#[test]
fn slot_labels_and_multiple_phases_filter_before_pagination() {
    let state = TaskState::new();
    for (name, environment, tier) in [
        ("a-match", "production", "frontend"),
        ("b-no-match", "development", "frontend"),
        ("c-match", "production", "backend"),
    ] {
        let mut labels = Labels::new();
        labels
            .insert("environment", environment)
            .insert("tier", tier);
        state
            .create_desired(
                &manifest(name, "primary", 1_000)
                    .with_labels(labels)
                    .unwrap(),
            )
            .unwrap();
    }
    let running = state.get(&TaskId::new("c-match").unwrap()).unwrap();
    let binding = bind(&state, running.name());
    assert!(state.transition_attempt_starting(&binding, 1));

    let selector: LabelSelector = "environment=production,tier in (frontend,backend)"
        .parse()
        .unwrap();
    let query = TaskQuery::new()
        .with_slot(Slot::new("primary").unwrap())
        .with_phases([TaskPhase::Pending, TaskPhase::Running])
        .with_label_selector(selector)
        .unwrap()
        .with_limit(1);

    let first = state.query(&query).unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].name().as_str(), "a-match");
    assert_eq!(first.remaining_item_count, 1);

    let second = state
        .query(&query.with_continuation(first.continuation.unwrap()))
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].name().as_str(), "c-match");
    assert_eq!(second.remaining_item_count, 0);
}

#[test]
fn metadata_apply_changes_label_query_membership_immediately() {
    let state = TaskState::new();
    let first = create(&state, "label-change");
    let query = TaskQuery::new()
        .with_label_selector("environment=production".parse().unwrap())
        .unwrap();
    assert!(state.query(&query).unwrap().items.is_empty());

    let mut labels = Labels::new();
    labels.insert("environment", "production");
    let applied = state
        .apply_desired(&TaskManifest::from(&first).with_labels(labels).unwrap())
        .unwrap()
        .task;

    assert_eq!(
        applied.metadata().generation(),
        first.metadata().generation()
    );
    assert_ne!(
        applied.metadata().resource_version(),
        first.metadata().resource_version()
    );
    assert_eq!(state.query(&query).unwrap().items.len(), 1);
}

#[test]
fn retention_uses_internal_terminal_timestamp() {
    let state = TaskState::new();
    let terminal = create(&state, "expired");
    let binding = bind(&state, terminal.name());
    assert_eq!(
        state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(terminal.name().clone())
    );
    create(&state, "pending");

    let config = StateConfig::new()
        .with_run_ttl(Duration::ZERO)
        .with_task_ttl(Duration::ZERO);
    assert_eq!(state.sweep(&config), (0, 1));
    assert!(!state.contains_task(&TaskId::new("expired").unwrap()));
    assert!(state.contains_task(&TaskId::new("pending").unwrap()));
}

#[test]
fn accepted_watch_history_capacity_constructs_state() {
    let config = StateConfig::new()
        .try_with_watch_history_capacity(3)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());

    assert_eq!(state.inner.read().watch_history_capacity, 3);
}

#[test]
fn task_level_completion_preserves_authoritative_attempt() {
    let state = TaskState::new();
    let task = create(&state, "final");
    let binding = bind(&state, task.name());
    assert!(state.transition_attempt_starting(&binding, 8));

    assert!(state.transition_task_finished(
        &binding,
        TaskPhase::Canceled,
        Some("controller stopped".into()),
        None,
    ));

    let stored = state.get(task.name()).unwrap();
    assert_eq!(stored.status().attempt(), 8);
    assert_eq!(stored.status().phase(), TaskPhase::Canceled);
}

#[test]
fn watch_history_retains_a_change_at_the_exact_byte_budget() {
    let task = journal_task("exact-budget", 1, 0);
    let serialized_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let config = StateConfig::new()
        .try_with_watch_history_byte_budget(serialized_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());

    record_current_change(&state, task);

    let inner = state.inner.read();
    assert_eq!(inner.watch_history.len(), 1);
    assert_eq!(inner.watch_history_bytes, serialized_bytes);
    assert_eq!(
        inner.watch_history.front().unwrap().serialized_bytes,
        serialized_bytes
    );
    assert_eq!(inner.compacted_through, 0);
}

#[test]
fn watch_history_byte_budget_can_evict_multiple_changes() {
    let first = journal_task("small-first", 1, 0);
    let second = journal_task("small-second", 2, 0);
    let third = journal_task("large-third", 3, 4 * 1024);
    let first_bytes = TaskState::serialized_task_payload_bytes(None, Some(&first));
    let second_bytes = TaskState::serialized_task_payload_bytes(None, Some(&second));
    let third_bytes = TaskState::serialized_task_payload_bytes(None, Some(&third));
    assert!(first_bytes + second_bytes <= third_bytes);
    let config = StateConfig::new()
        .try_with_watch_history_byte_budget(third_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());

    record_current_change(&state, first);
    record_current_change(&state, second);
    record_current_change(&state, third);

    let inner = state.inner.read();
    assert_eq!(inner.watch_history.len(), 1);
    assert_eq!(inner.watch_history.front().unwrap().revision, 3);
    assert_eq!(inner.watch_history_bytes, third_bytes);
    assert_eq!(inner.compacted_through, 2);
}

#[tokio::test]
async fn oversized_change_expires_existing_and_new_resume_points() {
    let first = journal_task("retained-before-oversized", 1, 0);
    let task = journal_task("oversized-live", 2, 4 * 1024);
    let serialized_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let config = StateConfig::new()
        .try_with_watch_history_byte_budget(serialized_bytes - 1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    record_current_change(&state, first);
    let mut watch = state.watch(&TaskFilter::new(), Some("epoch:1")).unwrap();

    record_current_change(&state, task.clone());

    {
        let inner = state.inner.read();
        assert!(inner.watch_history.is_empty());
        assert_eq!(inner.watch_history_bytes, 0);
        assert_eq!(inner.compacted_through, 2);
    }
    assert!(matches!(
        watch.next().await,
        Some(Err(CollectionError::ResourceVersionExpired { .. }))
    ));
    assert!(watch.next().await.is_none());
    assert!(matches!(
        state.watch(&TaskFilter::new(), Some("epoch:1")),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
}

#[test]
fn continuation_expires_after_byte_budget_compaction() {
    let config = StateConfig::new()
        .try_with_watch_history_byte_budget(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    create(&state, "a-first");
    create(&state, "b-second");
    let query = TaskQuery::new().with_limit(1);
    let first_page = state.query(&query).unwrap();
    let resource_version = first_page.resource_version.clone();
    let continuation = first_page.continuation.unwrap();

    create(&state, "c-third");

    assert_eq!(
        state
            .query(&query.with_continuation(continuation))
            .unwrap_err(),
        CollectionError::ResourceVersionExpired { resource_version }
    );
}

#[test]
fn list_snapshot_carries_the_atomic_collection_version() {
    let state = TaskState::new();
    let empty = state.query(&TaskQuery::new()).unwrap();
    let (epoch, revision) = TaskState::parse_resource_version(&empty.resource_version).unwrap();
    assert!(!epoch.is_empty());
    assert_eq!(revision, 0);

    let task = create(&state, "versioned-list");
    let page = state.query(&TaskQuery::new()).unwrap();
    assert_eq!(page.resource_version, task.metadata().resource_version());
    assert_eq!(page.items, vec![task]);
}

#[tokio::test]
async fn task_revision_rolls_epoch_and_expires_old_watch_and_cursor() {
    let state = TaskState::with_epoch(StateConfig::new(), "roll".to_string());
    create(&state, "a-first");
    let mut watch = state.watch(&TaskFilter::new(), Some("roll:1")).unwrap();
    let old_cursor =
        TaskContinuation::new("roll:1", TaskFilter::new(), TaskId::new("a-first").unwrap())
            .unwrap();

    {
        let mut inner = state.write(StateMutationEventCapacity::None);
        inner.resource_version = u64::MAX;
    }
    let second = create(&state, "b-second");

    assert_eq!(second.metadata().resource_version(), "next-roll:1");
    {
        let inner = state.inner.read();
        assert_eq!(inner.watch_history.len(), 1);
        assert_eq!(inner.watch_history.front().unwrap().revision, 1);
        assert_eq!(inner.compacted_through, 0);
    }
    assert!(matches!(
        watch.next().await,
        Some(Err(CollectionError::ResourceVersionExpired {
            resource_version
        })) if resource_version == "roll:1"
    ));
    assert!(matches!(
        state.query(&TaskQuery::new().with_continuation(old_cursor)),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
}

#[test]
fn continuation_reads_the_first_page_snapshot_after_live_changes() {
    let state = TaskState::new();
    let first_task = create(&state, "b-first");
    let second_task = create(&state, "c-second");
    let query = TaskQuery::new().with_limit(1);

    let first_page = state.query(&query).unwrap();
    assert_eq!(first_page.items, vec![first_task]);
    assert_eq!(first_page.remaining_item_count, 1);
    let continuation = first_page.continuation.unwrap();

    create(&state, "a-added-later");
    state
        .apply_desired(&manifest("c-second", "changed", 2_000))
        .unwrap();
    assert!(state.delete_task(second_task.name()));

    let second_page = state.query(&query.with_continuation(continuation)).unwrap();
    assert_eq!(second_page.resource_version, first_page.resource_version);
    assert_eq!(second_page.items, vec![second_task]);
    assert_eq!(second_page.remaining_item_count, 0);
    assert!(second_page.continuation.is_none());
}

#[test]
fn continuation_is_bound_to_its_filter_and_last_returned_name() {
    let state = TaskState::new();
    create(&state, "a-first");
    create(&state, "b-second");
    let query = TaskQuery::new().with_limit(1);
    let first_page = state.query(&query).unwrap();
    let continuation = first_page.continuation.unwrap();

    let mismatch = query
        .clone()
        .with_phase(TaskPhase::Running)
        .with_continuation(continuation.clone());
    assert_eq!(
        state.query(&mismatch).unwrap_err(),
        CollectionError::ContinuationFilterMismatch
    );

    let missing = TaskContinuation::new(
        continuation.resource_version(),
        query.filter().clone(),
        TaskId::new("missing-cursor").unwrap(),
    )
    .unwrap();
    assert_eq!(
        state.query(&query.with_continuation(missing)).unwrap_err(),
        CollectionError::ContinuationCursorNotFound {
            name: TaskId::new("missing-cursor").unwrap(),
        }
    );
}

#[test]
fn continuation_reports_invalid_and_expired_snapshots() {
    let config = StateConfig::new()
        .try_with_watch_history_capacity(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    create(&state, "a-first");
    create(&state, "b-second");
    let query = TaskQuery::new().with_limit(1);
    let first_page = state.query(&query).unwrap();
    let continuation = first_page.continuation.unwrap();

    let invalid = TaskContinuation::new(
        "epoch:99",
        query.filter().clone(),
        continuation.after().clone(),
    )
    .unwrap();
    assert_eq!(
        state
            .query(&query.clone().with_continuation(invalid))
            .unwrap_err(),
        CollectionError::InvalidResourceVersion {
            resource_version: "epoch:99".to_string(),
        }
    );

    create(&state, "c-third");
    create(&state, "d-fourth");
    assert_eq!(
        state
            .query(&query.with_continuation(continuation))
            .unwrap_err(),
        CollectionError::ResourceVersionExpired {
            resource_version: first_page.resource_version,
        }
    );
}

#[tokio::test]
async fn list_then_watch_replays_every_change_after_the_snapshot() {
    let state = TaskState::new();
    let listed = state.query(&TaskQuery::new()).unwrap();
    let created = create(&state, "created-in-gap");

    let mut watch = state
        .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
        .unwrap();
    let event = watch.next().await.unwrap().unwrap();

    assert_eq!(event, TaskWatchEvent::Added(created));
}

#[tokio::test]
async fn watch_without_version_emits_sorted_snapshot_then_live_changes() {
    let state = TaskState::new();
    let second = create(&state, "b-snapshot");
    let first = create(&state, "a-snapshot");
    let mut watch = state.watch(&TaskFilter::new(), None).unwrap();

    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(first)
    );
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(second)
    );

    let live = create(&state, "c-live");
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(live)
    );
}

#[tokio::test]
async fn watch_initial_replay_budget_admits_the_exact_boundary_and_releases_on_yield() {
    let task = journal_task("watch-byte-boundary", 1, 256);
    let task_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let config = StateConfig::new()
        .try_with_max_task_watch_initial_replay_bytes(task_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    record_current_change(&state, task.clone());

    let mut watch = state.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();
    assert_eq!(state.watch_admission.usage(), (1, task_bytes, 0));
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(task)
    );
    assert_eq!(state.watch_admission.usage(), (1, 0, 0));

    drop(watch);
    assert_eq!(state.watch_admission.usage(), (0, 0, 0));
}

#[test]
fn watch_initial_replay_budget_rejects_one_byte_over_without_side_effects() {
    let task = journal_task("watch-byte-rejected", 1, 256);
    let task_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let config = StateConfig::new()
        .try_with_max_task_watch_initial_replay_bytes(task_bytes - 1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    record_current_change(&state, task);
    let receivers_before = state.inner.read().watch_tx.receiver_count();

    let result = state.watch(&TaskFilter::new(), Some("epoch:0"));

    assert!(matches!(
        result,
        Err(CollectionError::TaskWatchInitialReplayByteLimitExceeded {
            current: 0,
            requested,
            limit,
        }) if requested == task_bytes && limit == task_bytes - 1
    ));
    assert_eq!(state.watch_admission.usage(), (0, 0, 0));
    assert_eq!(
        state.inner.read().watch_tx.receiver_count(),
        receivers_before
    );
    let inner = state.inner.read();
    assert_eq!(inner.resource_version, 1);
    assert_eq!(inner.watch_history.len(), 1);
}

#[test]
fn aggregate_watch_replay_bytes_reject_without_evicting_an_existing_watch() {
    let task = journal_task("watch-byte-aggregate", 1, 128);
    let task_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let config = StateConfig::new()
        .try_with_max_task_watch_initial_replay_bytes(task_bytes)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    record_current_change(&state, task);
    let first = state.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();

    let second = state.watch(&TaskFilter::new(), Some("epoch:0"));
    assert!(matches!(
        second,
        Err(CollectionError::TaskWatchInitialReplayByteLimitExceeded {
            current,
            requested,
            limit,
        }) if current == task_bytes && requested == task_bytes && limit == task_bytes
    ));
    assert_eq!(state.watch_admission.usage(), (1, task_bytes, 0));

    drop(first);
    assert_eq!(state.watch_admission.usage(), (0, 0, 0));
    assert!(state.watch(&TaskFilter::new(), Some("epoch:0")).is_ok());
}

#[test]
fn concurrent_watch_limit_is_atomic_and_drop_releases_the_lease() {
    const THREADS: usize = 16;

    let config = StateConfig::new()
        .try_with_max_concurrent_task_watches(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    let start = Arc::new(Barrier::new(THREADS + 1));
    let attempted = Arc::new(Barrier::new(THREADS + 1));
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();

    for _ in 0..THREADS {
        let state = state.clone();
        let start = Arc::clone(&start);
        let attempted = Arc::clone(&attempted);
        let accepted = Arc::clone(&accepted);
        threads.push(std::thread::spawn(move || {
            start.wait();
            let watch = state.watch(&TaskFilter::new(), Some("epoch:0")).ok();
            if watch.is_some() {
                accepted.fetch_add(1, Ordering::SeqCst);
            }
            attempted.wait();
            drop(watch);
        }));
    }

    start.wait();
    attempted.wait();
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(state.watch_admission.usage(), (0, 0, 0));
}

#[test]
fn count_rejection_does_not_run_the_predicate_or_retain_a_receiver() {
    let config = StateConfig::new()
        .try_with_max_concurrent_task_watches(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    create(&state, "count-held");
    let first = state.watch(&TaskFilter::new(), Some("0")).unwrap();
    let predicate_calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&predicate_calls);
    let receivers_before = state.inner.read().watch_tx.receiver_count();

    let rejected = state.watch_where(&TaskFilter::new(), Some("0"), move |_| {
        observed_calls.fetch_add(1, Ordering::SeqCst);
        true
    });

    assert!(matches!(
        rejected,
        Err(CollectionError::ConcurrentTaskWatchLimitReached { limit: 1 })
    ));
    assert_eq!(predicate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.inner.read().watch_tx.receiver_count(),
        receivers_before
    );
    assert_eq!(state.watch_admission.usage().0, 1);
    drop(first);
}

#[tokio::test]
async fn exact_resume_accounts_filter_relative_added_modified_and_deleted_events() {
    let state = TaskState::with_epoch(StateConfig::new(), "epoch".to_string());
    let first = create(&state, "replay-kinds");
    let filter = TaskFilter::new()
        .with_label_selector("environment=production".parse().unwrap())
        .unwrap();

    let mut labels = Labels::new();
    labels.insert("environment", "production");
    let added = state
        .apply_desired(&TaskManifest::from(&first).with_labels(labels).unwrap())
        .unwrap()
        .task;
    let mut annotations = Annotations::new();
    annotations.insert("example.io/revision", "2");
    let modified = state
        .apply_desired(
            &TaskManifest::from(&added)
                .with_annotations(annotations)
                .unwrap(),
        )
        .unwrap()
        .task;
    let current = state
        .apply_desired(
            &TaskManifest::from(&modified)
                .with_labels(Labels::new())
                .unwrap(),
        )
        .unwrap()
        .task;

    let mut watch = state.watch(&filter, Some("epoch:1")).unwrap();
    let buffered_bytes = watch
        .replay
        .iter()
        .filter_map(|change| change.event.as_ref())
        .map(|event| event.serialized_bytes)
        .sum::<usize>();
    assert_eq!(watch.replay.len(), 3);
    assert_eq!(state.watch_admission.usage(), (1, buffered_bytes, 0));

    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(added)
    );
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Modified(modified.clone())
    );
    let TaskWatchEvent::Deleted(deleted) = watch.next().await.unwrap().unwrap() else {
        panic!("filter exit must replay as Deleted");
    };
    assert_eq!(deleted.name(), current.name());
    assert_eq!(deleted.metadata().labels(), modified.metadata().labels());
    assert_eq!(deleted.metadata().resource_version(), "epoch:4");
    assert_eq!(state.watch_admission.usage(), (1, 0, 0));
}

#[test]
fn deleted_watch_byte_probe_matches_the_materialized_event() {
    let previous = journal_task("deleted-byte-probe", 9, 128);
    let resource_version = "epoch:10";
    let probed =
        TaskState::serialized_task_with_resource_version_bytes(&previous, resource_version);
    let mut materialized = previous;
    materialized.set_resource_version(resource_version).unwrap();

    assert_eq!(
        probed,
        TaskState::serialized_task_payload_bytes(None, Some(&materialized))
    );
}

#[test]
fn watch_rejects_expired_invalid_and_foreign_versions() {
    let config = StateConfig::new()
        .try_with_watch_history_capacity(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    create(&state, "first");
    create(&state, "second");

    assert!(matches!(
        state.watch(&TaskFilter::new(), Some("epoch:0")),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
    assert!(matches!(
        state.watch(&TaskFilter::new(), Some("another:2")),
        Err(CollectionError::ResourceVersionExpired { .. })
    ));
    assert!(matches!(
        state.watch(&TaskFilter::new(), Some("")),
        Err(CollectionError::InvalidResourceVersion { .. })
    ));
    assert!(matches!(
        state.watch(&TaskFilter::new(), Some("epoch:not-a-number")),
        Err(CollectionError::InvalidResourceVersion { .. })
    ));
    assert!(matches!(
        state.watch(&TaskFilter::new(), Some("epoch:3")),
        Err(CollectionError::InvalidResourceVersion { .. })
    ));
}

#[tokio::test]
async fn selector_membership_changes_map_to_added_modified_and_deleted() {
    let state = TaskState::new();
    let first = create(&state, "selector-transition");
    let filter = TaskFilter::new()
        .with_label_selector("environment=production".parse().unwrap())
        .unwrap();
    let listed = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch(&filter, Some(listed.resource_version.as_str()))
        .unwrap();

    let mut labels = Labels::new();
    labels.insert("environment", "production");
    let added = state
        .apply_desired(&TaskManifest::from(&first).with_labels(labels).unwrap())
        .unwrap()
        .task;
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(added.clone())
    );

    let mut annotations = Annotations::new();
    annotations.insert("example.io/revision", "2");
    let modified = state
        .apply_desired(
            &TaskManifest::from(&added)
                .with_annotations(annotations)
                .unwrap(),
        )
        .unwrap()
        .task;
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Modified(modified.clone())
    );

    let current = state
        .apply_desired(
            &TaskManifest::from(&modified)
                .with_labels(Labels::new())
                .unwrap(),
        )
        .unwrap()
        .task;
    let deleted = watch.next().await.unwrap().unwrap();
    let TaskWatchEvent::Deleted(deleted) = deleted else {
        panic!("selector exit must be Deleted");
    };
    assert_eq!(deleted.name(), current.name());
    assert_eq!(deleted.metadata().labels(), modified.metadata().labels());
    assert_eq!(
        deleted.metadata().resource_version(),
        current.metadata().resource_version()
    );
}

#[tokio::test]
async fn adapter_predicate_participates_in_watch_transitions() {
    let state = TaskState::new();
    let first = create(&state, "visibility-transition");
    let listed = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch_where(
            &TaskFilter::new(),
            Some(listed.resource_version.as_str()),
            |task| task.spec().timeout().as_millis() <= 1_000,
        )
        .unwrap();

    let hidden = state
        .apply_desired(&TaskManifest::new(first.name().as_str(), spec("slot", 2_000)).unwrap())
        .unwrap()
        .task;
    let TaskWatchEvent::Deleted(deleted) = watch.next().await.unwrap().unwrap() else {
        panic!("leaving adapter visibility must be Deleted");
    };
    assert_eq!(deleted.name(), hidden.name());
    assert_eq!(
        deleted.metadata().resource_version(),
        hidden.metadata().resource_version()
    );

    let visible = state
        .apply_desired(&TaskManifest::new(first.name().as_str(), spec("slot", 1_000)).unwrap())
        .unwrap()
        .task;
    assert_eq!(
        watch.next().await.unwrap().unwrap(),
        TaskWatchEvent::Added(visible)
    );
}

#[test]
fn initial_watch_predicate_runs_without_the_state_lock() {
    let state = TaskState::with_epoch(StateConfig::default(), "epoch".to_string());
    create(&state, "visible");
    let inner = Arc::clone(&state.inner);
    let lock_was_free = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&lock_was_free);

    let _watch = state
        .watch_where(&TaskFilter::new(), None, move |_| {
            observed.store(inner.try_write().is_some(), Ordering::SeqCst);
            true
        })
        .unwrap();

    assert!(lock_was_free.load(Ordering::SeqCst));
}

#[tokio::test]
async fn delete_and_sweep_each_publish_one_deleted_event() {
    let state = TaskState::new();
    let deleted = create(&state, "api-deleted");
    let listed = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
        .unwrap();
    assert!(state.delete_task(deleted.name()));
    let TaskWatchEvent::Deleted(event) = watch.next().await.unwrap().unwrap() else {
        panic!("delete must emit Deleted");
    };
    assert_eq!(event.name(), deleted.name());
    assert_ne!(
        event.metadata().resource_version(),
        deleted.metadata().resource_version()
    );

    let expired = create(&state, "sweep-deleted");
    let binding = bind(&state, expired.name());
    assert_eq!(
        state.finalize_if_bound(
            binding.tv.get(),
            TaskPhase::Canceled,
            Some("canceled".into()),
            None,
            true,
        ),
        Some(expired.name().clone())
    );
    let listed = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
        .unwrap();
    let config = StateConfig::new()
        .with_run_ttl(Duration::ZERO)
        .with_task_ttl(Duration::ZERO);
    assert_eq!(state.sweep(&config), (0, 1));
    let TaskWatchEvent::Deleted(event) = watch.next().await.unwrap().unwrap() else {
        panic!("sweep must emit Deleted");
    };
    assert_eq!(event.name(), expired.name());
}

#[tokio::test]
async fn no_op_apply_and_run_only_change_publish_nothing() {
    let state = TaskState::new();
    let first = create(&state, "no-watch-noop");
    let binding = bind(&state, first.name());
    assert!(state.transition_attempt_starting(&binding, 1));

    let changed = TaskManifest::new(first.name().as_str(), spec("slot", 2_000)).unwrap();
    let current = state.apply_desired(&changed).unwrap().task;
    let listed = state.query(&TaskQuery::new()).unwrap();
    let mut watch = state
        .watch(&TaskFilter::new(), Some(listed.resource_version.as_str()))
        .unwrap();

    let noop = state.apply_desired(&TaskManifest::from(&current)).unwrap();
    assert!(!noop.reconcile);
    assert!(state.transition_attempt_finished(&binding, 1, TaskPhase::Succeeded, None, None,));
    assert_eq!(
        state.query(&TaskQuery::new()).unwrap().resource_version,
        listed.resource_version
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), watch.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lag_is_terminal_once_the_resume_point_is_compacted() {
    let config = StateConfig::new()
        .try_with_watch_history_capacity(1)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    let mut watch = state.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();

    create(&state, "first");
    create(&state, "second");

    assert!(matches!(
        watch.next().await,
        Some(Err(CollectionError::ResourceVersionExpired { .. }))
    ));
    assert!(watch.next().await.is_none());
}

#[test]
fn coalesced_revision_notification_replays_retained_changes_exactly_once() {
    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let config = StateConfig::new()
        .try_with_watch_history_capacity(3)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    let mut watch = state.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();
    let tasks = [
        journal_task("coalesced-first", 1, 0),
        journal_task("coalesced-second", 2, 0),
        journal_task("coalesced-third", 3, 0),
    ];
    for task in tasks.iter().cloned() {
        record_current_change(&state, task);
    }
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);

    for expected in &tasks {
        assert!(matches!(
            Pin::new(&mut watch).poll_next(&mut context),
            Poll::Ready(Some(Ok(TaskWatchEvent::Added(task)))) if task == *expected
        ));
    }
    assert!(matches!(
        Pin::new(&mut watch).poll_next(&mut context),
        Poll::Pending
    ));
    assert_eq!(watch.last_revision, 3);
    assert_eq!(watch.target_revision, 3);
}

#[test]
fn coalesced_revision_notification_advances_across_a_trailing_gap() {
    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let config = StateConfig::new()
        .try_with_watch_history_capacity(3)
        .unwrap();
    let state = TaskState::with_epoch(config, "epoch".to_string());
    let task = create(&state, "coalesced-gap-resource");
    let binding = bind(&state, task.name());
    assert!(state.mark_observed(&binding.resource));
    let mut watch = state.watch(&TaskFilter::new(), Some("epoch:2")).unwrap();
    let changes = [
        journal_task("coalesced-gap-first", 3, 0),
        journal_task("coalesced-gap-second", 4, 0),
        journal_task("coalesced-gap-third", 5, 0),
    ];
    for change in changes.iter().cloned() {
        record_current_change(&state, change);
    }
    assert!(!state.mark_observed(&binding.resource));
    assert_eq!(state.inner.read().resource_version, 6);

    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    for expected in &changes {
        assert!(matches!(
            Pin::new(&mut watch).poll_next(&mut context),
            Poll::Ready(Some(Ok(TaskWatchEvent::Added(task)))) if task == *expected
        ));
    }
    assert!(matches!(
        Pin::new(&mut watch).poll_next(&mut context),
        Poll::Pending
    ));
    assert_eq!(watch.last_revision, 6);
    assert_eq!(watch.target_revision, 6);
}

#[test]
fn slow_live_watchers_retain_no_payload_outside_the_shared_journal() {
    let state = TaskState::with_epoch(StateConfig::new(), "epoch".to_string());
    let watchers = (0..64)
        .map(|_| state.watch(&TaskFilter::new(), Some("epoch:0")).unwrap())
        .collect::<Vec<_>>();
    let task = journal_task("shared-live-payload", 1, 16 * 1024);

    record_current_change(&state, task);

    let change = state
        .inner
        .read()
        .watch_history
        .front()
        .cloned()
        .expect("the live change is retained once by the journal");
    assert_eq!(Arc::strong_count(&change), 2);
    assert_eq!(
        Arc::strong_count(change.current.as_ref().expect("current payload")),
        1
    );
    assert_eq!(state.watch_admission.usage(), (64, 0, 0));

    drop(watchers);
    assert_eq!(Arc::strong_count(&change), 2);
    assert_eq!(
        Arc::strong_count(change.current.as_ref().expect("current payload")),
        1
    );
    assert_eq!(state.watch_admission.usage(), (0, 0, 0));
}

#[test]
fn live_watch_byte_budget_exact_boundary_delivers_and_one_byte_under_expires() {
    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let task = journal_task("live-byte-boundary", 1, 512);
    let task_bytes = TaskState::serialized_task_payload_bytes(None, Some(&task));
    let exact_config = StateConfig::new()
        .try_with_watch_history_byte_budget(task_bytes)
        .unwrap();
    let exact = TaskState::with_epoch(exact_config, "epoch".to_string());
    let mut exact_watch = exact.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();
    record_current_change(&exact, task.clone());

    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut exact_watch).poll_next(&mut context),
        Poll::Ready(Some(Ok(TaskWatchEvent::Added(event)))) if event == task
    ));
    assert_eq!(exact.inner.read().watch_history_bytes, task_bytes);

    let under_config = StateConfig::new()
        .try_with_watch_history_byte_budget(task_bytes - 1)
        .unwrap();
    let under = TaskState::with_epoch(under_config, "epoch".to_string());
    let mut under_watch = under.watch(&TaskFilter::new(), Some("epoch:0")).unwrap();
    record_current_change(&under, task);
    assert!(matches!(
        Pin::new(&mut under_watch).poll_next(&mut context),
        Poll::Ready(Some(Err(CollectionError::ResourceVersionExpired { .. })))
    ));
    assert!(under.inner.read().watch_history.is_empty());
    assert_eq!(under.watch_admission.usage(), (0, 0, 0));
}
#[test]
fn irrelevant_live_events_yield_after_the_poll_budget() {
    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let state = TaskState::new();
    let listed = state.query(&TaskQuery::new()).unwrap();
    let filter = TaskFilter::new()
        .with_label_selector("watched=true".parse().unwrap())
        .unwrap();
    let mut watch = state
        .watch(&filter, Some(listed.resource_version.as_str()))
        .unwrap();
    for index in 0..=WATCH_POLL_BUDGET {
        create(&state, &format!("irrelevant-{index}"));
    }

    let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake));
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut watch).poll_next(&mut context),
        Poll::Pending
    ));
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);

    state.close_watches();
    assert!(matches!(
        Pin::new(&mut watch).poll_next(&mut context),
        Poll::Ready(None)
    ));
}

#[tokio::test]
async fn closing_state_watches_ends_the_stream() {
    let state = TaskState::new();
    let mut watch = state.watch(&TaskFilter::new(), Some("0")).unwrap();
    state.close_watches();
    assert!(watch.next().await.is_none());
}
