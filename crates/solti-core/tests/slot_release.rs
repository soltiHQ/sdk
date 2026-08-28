//! SDK diagnostics for reusing a DropIfRunning slot after logical settlement.
//!
//! Succeeded, cancel, delete, TaskFinished, and destruction of a task value are
//! not exposed controller-Idle barriers. These tests accept a typed SlotBusy
//! rejection, but require an exact matching SDK state and no invented attempt.
//! Every submission is a new generation or resource incarnation, never a retry.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use solti_core::{StateConfig, SupervisorApi};
use solti_model::{
    AdmissionPolicy, EmbeddedSpec, RestartPolicy, Task, TaskFilter, TaskId, TaskManifest,
    TaskPhase, TaskRun, TaskRunQuery, TaskSpec, TaskWorkload, Uid,
};
use solti_runner::RunnerRouter;
use taskvisor::{
    BoxTaskFuture, Event, EventKind, RejectionKind, Subscribe, SupervisorConfig, TaskContext,
    TaskError, TaskOutcomeKind, TaskRef,
};
use tokio_stream::StreamExt;

const NAME: &str = "slot-release-resource";
const SLOT: &str = "slot-release-slot";
const GENERATIONS: usize = 1024;
const INCARNATIONS: usize = 256;
const TRACE_CAPACITY: usize = 32 * 1024;
// Failure bounds for this finite fixture, not claimed SDK service deadlines.
const OPERATION_BOUND: Duration = Duration::from_secs(5);
const CASE_BOUND: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reuse {
    ApplyGeneration,
    DeleteRecreate,
}

impl Reuse {
    fn iterations(self) -> usize {
        match self {
            Self::ApplyGeneration => GENERATIONS,
            Self::DeleteRecreate => INCARNATIONS,
        }
    }
}

#[derive(Default)]
struct Capture {
    events: Mutex<Vec<Event>>,
    overflowed: AtomicBool,
}

impl Subscribe for Capture {
    fn name(&self) -> &str {
        "sdk-slot-release-diagnostic"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(TRACE_CAPACITY).unwrap()
    }

    fn on_event(&self, event: &Event) {
        let mut events = self.events.lock().unwrap();
        if events.len() == TRACE_CAPACITY {
            self.overflowed.store(true, Ordering::SeqCst);
        } else {
            events.push(event.clone());
        }
    }
}

#[derive(Default, Debug)]
struct Counters {
    spawned: AtomicUsize,
    polled: AtomicUsize,
    future_dropped: AtomicUsize,
    task_dropped: AtomicUsize,
}

#[derive(Clone, Copy, Debug)]
struct DropObservation {
    future_dropped: usize,
    task_dropped: usize,
}

impl Counters {
    fn drops(&self) -> DropObservation {
        DropObservation {
            future_dropped: self.future_dropped.load(Ordering::SeqCst),
            task_dropped: self.task_dropped.load(Ordering::SeqCst),
        }
    }
}

struct ImmediateTask(Arc<Counters>);

impl taskvisor::Task for ImmediateTask {
    fn spawn(&self, _context: TaskContext) -> BoxTaskFuture {
        self.0.spawned.fetch_add(1, Ordering::SeqCst);
        Box::pin(ImmediateFuture(Arc::clone(&self.0)))
    }
}

impl Drop for ImmediateTask {
    fn drop(&mut self) {
        self.0.task_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct ImmediateFuture(Arc<Counters>);

impl Future for ImmediateFuture {
    type Output = Result<(), TaskError>;

    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.polled.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}

impl Drop for ImmediateFuture {
    fn drop(&mut self) {
        self.0.future_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct Observation {
    committed: Task,
    terminal: Task,
    settled: Task,
    counters: Arc<Counters>,
    previous_at_submit: Option<DropObservation>,
}

fn manifest(revision: usize) -> TaskManifest {
    TaskManifest::new(
        NAME,
        TaskSpec::builder(
            SLOT,
            TaskWorkload::Embedded(EmbeddedSpec::new(format!("slot-release-{revision}")).unwrap()),
            30_000_u64,
        )
        .admission(AdmissionPolicy::DropIfRunning)
        .restart(RestartPolicy::Never)
        .build()
        .unwrap(),
    )
    .unwrap()
}

fn is_current_terminal(task: &Task, committed: &Task) -> bool {
    task.name() == committed.name()
        && task.uid() == committed.uid()
        && task.metadata().generation() == committed.metadata().generation()
        && task.phase().is_terminal()
}

async fn terminal(api: &SupervisorApi, committed: &Task) -> Result<Task, String> {
    let mut watch = api
        .watch_tasks(&TaskFilter::new().with_slot(committed.slot().clone()), None)
        .map_err(|error| format!("open task watch: {error}"))?;
    if let Some(task) = api.get_task(committed.name())
        && is_current_terminal(&task, committed)
    {
        return Ok(task);
    }
    tokio::time::timeout(OPERATION_BOUND, async {
        while let Some(event) = watch.next().await {
            let task = event
                .map_err(|error| format!("task watch: {error}"))?
                .into_object();
            if is_current_terminal(&task, committed) {
                return Ok(task);
            }
        }
        Err("task watch closed before the current terminal state".to_owned())
    })
    .await
    .map_err(|_| {
        format!(
            "terminal timeout: uid={} generation={} current={:?}",
            committed.uid(),
            committed.metadata().generation(),
            api.get_task(committed.name())
        )
    })?
}

fn history(api: &SupervisorApi, task: &Task) -> Result<Vec<TaskRun>, String> {
    let base = TaskRunQuery::new().with_limit(128);
    let mut query = base.clone();
    let mut version = None;
    let mut runs = Vec::new();
    // At most 1024 attempts are possible. Bound pagination independently of
    // the async deadline in case a broken continuation stops advancing.
    for _ in 0..=GENERATIONS / 128 {
        let page = api
            .query_task_runs(task.name(), &query)
            .map_err(|error| format!("history query: {error}"))?
            .ok_or_else(|| "history resource disappeared".to_owned())?;
        if page.task != *task.name() || page.task_uid != *task.uid() {
            return Err(format!("history has the wrong resource identity: {page:?}"));
        }
        if let Some(expected) = &version {
            if &page.resource_version != expected {
                return Err("history changed snapshot while paging".to_owned());
            }
        } else {
            version = Some(page.resource_version.clone());
        }
        runs.extend(page.items);
        if runs.len() > GENERATIONS {
            return Err("history contains more runs than submitted work".to_owned());
        }
        match page.continuation {
            Some(continuation) => query = base.clone().with_continuation(continuation),
            None => return Ok(runs),
        }
    }
    Err("history pagination did not terminate".to_owned())
}

async fn cycles(
    api: &SupervisorApi,
    reuse: Reuse,
    observations: &mut Vec<Observation>,
    runs: &mut Vec<(Uid, TaskRun)>,
) -> Result<(), String> {
    for iteration in 0..reuse.iterations() {
        let counters = Arc::new(Counters::default());
        let task: TaskRef = Arc::new(ImmediateTask(Arc::clone(&counters)));
        let previous_at_submit = observations
            .last()
            .map(|previous| previous.counters.drops());
        let committed = if reuse == Reuse::ApplyGeneration && iteration > 0 {
            api.apply_embedded_task(manifest(iteration + 1), task).await
        } else {
            api.create_embedded_task(manifest(1), task).await
        }
        .map_err(|error| format!("desired-state commit at iteration {iteration}: {error}"))?;
        let terminal = terminal(api, &committed).await?;
        api.cancel_task(committed.name())
            .await
            .map_err(|error| format!("logical cancel settlement: {error}"))?;
        let settled = api
            .get_task(committed.name())
            .ok_or_else(|| "settled task disappeared".to_owned())?;

        if reuse == Reuse::DeleteRecreate {
            runs.extend(
                history(api, &settled)?
                    .into_iter()
                    .map(|run| (settled.uid().clone(), run)),
            );
        }
        observations.push(Observation {
            committed,
            terminal,
            settled,
            counters,
            previous_at_submit,
        });
        if reuse == Reuse::DeleteRecreate {
            api.delete_task(observations.last().unwrap().settled.name())
                .await
                .map_err(|error| format!("delete before next incarnation: {error}"))?;
        }
    }

    // Do not add an event-drain or history barrier between cancel settlement and
    // the next generation. Preserve every run until this final paged snapshot.
    if reuse == Reuse::ApplyGeneration {
        let task = &observations.last().unwrap().settled;
        runs.extend(
            history(api, task)?
                .into_iter()
                .map(|run| (task.uid().clone(), run)),
        );
    }
    Ok(())
}

async fn diagnostic(reuse: Reuse, runtime: &str) {
    let capture = Arc::new(Capture::default());
    let api = tokio::time::timeout(
        OPERATION_BOUND,
        SupervisorApi::builder(RunnerRouter::new())
            .with_runtime_config(
                SupervisorConfig::default()
                    .with_bus_capacity(NonZeroUsize::new(TRACE_CAPACITY).unwrap()),
            )
            .with_state_config(StateConfig::default().with_max_runs_per_task(GENERATIONS))
            .with_subscribers(vec![capture.clone()])
            .start(),
    )
    .await
    .expect("SDK startup timed out")
    .expect("SDK startup failed");
    let mut observations = Vec::with_capacity(reuse.iterations());
    let mut runs = Vec::new();
    let started = Instant::now();
    let result = tokio::time::timeout(
        CASE_BOUND,
        cycles(&api, reuse, &mut observations, &mut runs),
    )
    .await;

    // Cleanup also runs when observation or management returns an error or the
    // finite cycle deadline expires. Assert diagnostic results only afterward.
    let name = TaskId::new(NAME).unwrap();
    let deleted = tokio::time::timeout(OPERATION_BOUND, async {
        if api.get_task(&name).is_some() {
            api.delete_task(&name).await?;
        }
        Ok::<(), solti_core::CoreError>(())
    })
    .await;
    let shutdown = tokio::time::timeout(OPERATION_BOUND, api.shutdown()).await;
    let dropped = tokio::time::timeout(OPERATION_BOUND, async {
        while observations
            .iter()
            .any(|observation| observation.counters.task_dropped.load(Ordering::SeqCst) == 0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let events = capture.events.lock().unwrap().clone();
    let accepted = events
        .iter()
        .filter(|event| event.kind == EventKind::ControllerSubmitted)
        .count();
    let rejected = events
        .iter()
        .filter(|event| event.kind == EventKind::ControllerRejected)
        .count();
    eprintln!(
        "SDK slot release {reuse:?}/{runtime}: observations={}, submitted={accepted}, \
         rejected={rejected}, history={}, elapsed={:?}; cycles={result:?}, \
         delete={deleted:?}, shutdown={shutdown:?}, task_values_dropped={dropped:?}",
        observations.len(),
        runs.len(),
        started.elapsed()
    );

    deleted
        .expect("final delete timed out")
        .expect("final delete failed");
    shutdown
        .expect("SDK shutdown timed out")
        .expect("SDK shutdown failed");
    dropped.expect("ordinary task values did not finish destruction after shutdown");
    result
        .expect("diagnostic cycle deadline expired")
        .expect("diagnostic cycle failed");
    assert!(api.get_task(&name).is_none());
    assert!(
        !capture.overflowed.load(Ordering::SeqCst),
        "diagnostic trace overflowed"
    );
    assert_eq!(observations.len(), reuse.iterations());
    validate(reuse, runtime, &observations, &runs, &events);
}

fn validate(
    reuse: Reuse,
    runtime: &str,
    observations: &[Observation],
    runs: &[(Uid, TaskRun)],
    events: &[Event],
) {
    for event in events {
        assert!(
            !matches!(
                event.kind,
                EventKind::SubscriberOverflow
                    | EventKind::SubscriberPanicked
                    | EventKind::RuntimeFailure
                    | EventKind::OwnershipCapacityRetired
                    | EventKind::GraceExceeded
            ),
            "fixture lost trace or failed cleanup: {event:?}"
        );
    }

    // Only one SDK submission is outstanding at a time. The single controller
    // publishes admissions serially; this subscriber preserves callback FIFO.
    // Correlate after shutdown rather than adding an admission wait between
    // cycles. SDK state/history supply UID+generation; events supply TV id+slot.
    let admissions: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::ControllerSubmitted | EventKind::ControllerRejected
            )
        })
        .collect();
    assert_eq!(
        admissions.len(),
        observations.len(),
        "missing or duplicate admission"
    );
    assert_eq!(
        admissions[0].kind,
        EventKind::ControllerSubmitted,
        "initial slot is unused"
    );
    let mut ids = HashSet::new();
    let mut uids = HashSet::new();
    let mut traces: HashMap<taskvisor::TaskId, Vec<&Event>> = HashMap::new();
    for event in events {
        if let Some(id) = event.id {
            traces.entry(id).or_default().push(event);
        }
    }
    let mut history: HashMap<(Uid, u64), Vec<&TaskRun>> = HashMap::new();
    for (uid, run) in runs {
        history
            .entry((uid.clone(), run.generation()))
            .or_default()
            .push(run);
    }
    let mut successes = 0;
    let mut busy = 0;
    let mut busy_after_previous_task_drop = 0;

    for (index, (observation, admission)) in observations.iter().zip(&admissions).enumerate() {
        let id = admission
            .id
            .expect("admission must carry its exact Taskvisor id");
        assert!(ids.insert(id), "duplicate Taskvisor id {id:?}");
        assert_eq!(admission.task.as_deref(), Some(SLOT));
        let committed = &observation.committed;
        let settled = &observation.settled;
        let generation = committed.metadata().generation();
        let trace = &traces[&id];
        let detail = format!(
            "{reuse:?}/{runtime} index={index} uid={} generation={generation} tv_id={id:?} \
             slot={SLOT}; observation={observation:?}; trace={trace:?}",
            committed.uid()
        );
        assert_eq!(
            committed.spec().admission(),
            AdmissionPolicy::DropIfRunning,
            "{detail}"
        );
        assert_eq!(committed.name().as_str(), NAME, "{detail}");
        assert_eq!(committed.slot().as_str(), SLOT, "{detail}");
        assert!(
            is_current_terminal(&observation.terminal, committed),
            "{detail}"
        );
        assert!(is_current_terminal(settled, committed), "{detail}");
        assert_eq!(
            settled.status().observed_generation(),
            generation,
            "{detail}"
        );
        assert_eq!(
            settled.status().reconciled().observed_generation(),
            generation,
            "{detail}"
        );
        assert!(!settled.status().reconciliation_failed(), "{detail}");
        assert_eq!(settled.status().exit_code(), None, "{detail}");
        match reuse {
            Reuse::ApplyGeneration => {
                assert_eq!(committed.uid(), observations[0].committed.uid(), "{detail}");
                assert_eq!(generation, index as u64 + 1, "{detail}");
            }
            Reuse::DeleteRecreate => {
                assert!(
                    uids.insert(committed.uid().clone()),
                    "reused resource UID: {detail}"
                );
                assert_eq!(generation, 1, "{detail}");
            }
        }
        let matched_runs = history
            .remove(&(committed.uid().clone(), generation))
            .unwrap_or_default();
        let count = |kind| trace.iter().filter(|event| event.kind == kind).count();
        match admission.kind {
            EventKind::ControllerSubmitted => {
                successes += 1;
                assert_eq!(settled.phase(), &TaskPhase::Succeeded, "{detail}");
                assert_eq!(
                    observation.terminal.phase(),
                    &TaskPhase::Succeeded,
                    "{detail}"
                );
                assert_eq!(settled.status().attempt(), 1, "{detail}");
                assert_eq!(settled.status().error(), None, "{detail}");
                assert_eq!(
                    matched_runs.len(),
                    1,
                    "success needs exactly one history run: {detail}"
                );
                let run = matched_runs[0];
                assert_eq!(run.attempt(), 1, "{detail}");
                assert_eq!(run.phase(), TaskPhase::Succeeded, "{detail}");
                assert!(!run.is_active(), "{detail}");
                assert_eq!(run.error(), None, "{detail}");
                for kind in [
                    EventKind::TaskAddRequested,
                    EventKind::TaskAdded,
                    EventKind::AttemptStarting,
                    EventKind::AttemptSucceeded,
                    EventKind::TaskFinished,
                    EventKind::TaskRemoved,
                ] {
                    assert_eq!(count(kind), 1, "missing or duplicate {kind:?}: {detail}");
                }
                let finished = trace
                    .iter()
                    .find(|event| event.kind == EventKind::TaskFinished)
                    .unwrap();
                assert_eq!(
                    finished.outcome_kind,
                    Some(TaskOutcomeKind::Completed),
                    "{detail}"
                );
                for event in trace.iter().filter(|event| {
                    matches!(
                        event.kind,
                        EventKind::AttemptStarting | EventKind::AttemptSucceeded
                    )
                }) {
                    assert_eq!(event.attempt, Some(1), "{detail}");
                    assert_eq!(
                        event.task, finished.task,
                        "runtime label/id mismatch: {detail}"
                    );
                }
            }
            EventKind::ControllerRejected => {
                busy += 1;
                assert_eq!(
                    admission.rejection_kind,
                    Some(RejectionKind::SlotBusy),
                    "{detail}"
                );
                assert_eq!(
                    admission.outcome_kind,
                    Some(TaskOutcomeKind::Rejected),
                    "{detail}"
                );
                assert_eq!(
                    settled.phase(),
                    &TaskPhase::Canceled,
                    "unexplained Canceled: {detail}"
                );
                assert_eq!(
                    observation.terminal.phase(),
                    &TaskPhase::Canceled,
                    "{detail}"
                );
                assert_eq!(
                    settled.status().attempt(),
                    0,
                    "rejected work started: {detail}"
                );
                assert!(
                    admission.reason.is_some(),
                    "rejection lost its diagnostic: {detail}"
                );
                assert_eq!(
                    settled.status().error(),
                    admission.reason.as_deref(),
                    "{detail}"
                );
                assert!(
                    matched_runs.is_empty(),
                    "rejected work invented history: {detail}"
                );
                for kind in [
                    EventKind::TaskAddRequested,
                    EventKind::TaskAdded,
                    EventKind::AttemptStarting,
                    EventKind::AttemptSucceeded,
                    EventKind::TaskFinished,
                    EventKind::TaskRemoved,
                ] {
                    assert_eq!(count(kind), 0, "rejected work produced {kind:?}: {detail}");
                }
                if let Some(previous) = observation.previous_at_submit {
                    if previous.task_dropped == 1 {
                        busy_after_previous_task_drop += 1;
                    }
                    if busy <= 4 {
                        eprintln!(
                            "SDK SlotBusy {reuse:?}/{runtime}: uid={} generation={generation} \
                             id={id:?} slot={SLOT} previous_id={:?} \
                             previous_future_dropped={} previous_task_dropped={}; \
                             these destruction signals do not prove controller Idle",
                            committed.uid(),
                            admissions[index - 1].id,
                            previous.future_dropped,
                            previous.task_dropped,
                        );
                    }
                }
            }
            _ => unreachable!("admissions were filtered by kind"),
        }
        for kind in [
            EventKind::TaskAddFailed,
            EventKind::AttemptCanceled,
            EventKind::AttemptFailed,
            EventKind::AttemptTimedOut,
            EventKind::BackoffScheduled,
        ] {
            assert_eq!(count(kind), 0, "unexpected {kind:?}: {detail}");
        }
        let expected_attempts = usize::from(admission.kind == EventKind::ControllerSubmitted);
        assert_eq!(
            observation.counters.spawned.load(Ordering::SeqCst),
            expected_attempts,
            "{detail}"
        );
        assert_eq!(
            observation.counters.polled.load(Ordering::SeqCst),
            expected_attempts,
            "{detail}"
        );
        assert_eq!(
            observation.counters.future_dropped.load(Ordering::SeqCst),
            expected_attempts,
            "{detail}"
        );
        assert_eq!(
            observation.counters.task_dropped.load(Ordering::SeqCst),
            1,
            "{detail}"
        );
    }
    assert!(
        history.is_empty(),
        "history has an unsubmitted UID/generation: {history:?}"
    );
    assert!(
        traces.keys().all(|id| ids.contains(id)),
        "trace has an uncorrelated Taskvisor id"
    );
    eprintln!(
        "SDK verified {reuse:?}/{runtime}: Completed={successes}, typed_SlotBusy={busy}, \
         SlotBusy_after_previous_task_value_drop={busy_after_previous_task_drop}; \
         no admission retries, no Queue, no controller-Idle claim"
    );
}

#[tokio::test]
async fn same_name_generations_after_logical_settlement_current_thread() {
    diagnostic(Reuse::ApplyGeneration, "current_thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_name_generations_after_logical_settlement_multi_thread() {
    diagnostic(Reuse::ApplyGeneration, "multi_thread").await;
}

#[tokio::test]
async fn deleted_name_recreates_reused_slot_current_thread() {
    diagnostic(Reuse::DeleteRecreate, "current_thread").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_name_recreates_reused_slot_multi_thread() {
    diagnostic(Reuse::DeleteRecreate, "multi_thread").await;
}
