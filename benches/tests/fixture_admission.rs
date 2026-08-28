//! Regression coverage for success fixtures that reuse a controller slot.

use std::sync::Arc;

use solti_benches::fixtures::{
    bounded, current_thread, embedded_manifest, multi_thread, wait_task, wait_terminal,
};
use solti_core::SupervisorApi;
use solti_model::{AdmissionPolicy, Task, TaskManifest, TaskPhase, TaskRun, TaskRunQuery};
use solti_runner::RunnerRouter;
use taskvisor::{Event, EventKind, Subscribe};
use tokio::sync::mpsc;

#[path = "../scenarios/core_support/mod.rs"]
mod core_support;

#[cfg(feature = "http")]
#[allow(dead_code)]
#[path = "../scenarios/boundary_support/http.rs"]
mod http_support;

use core_support::{
    ControlledRunner, Counter, embedded_revision, held_task, immediate_task, marked_task,
    retained_task, routed_manifest, router,
};

struct AdmissionEvents {
    sender: mpsc::Sender<Event>,
}

impl Subscribe for AdmissionEvents {
    fn name(&self) -> &'static str {
        "fixture-admission-regression"
    }

    fn on_event(&self, event: &Event) {
        if matches!(
            event.kind,
            EventKind::ControllerSubmitted | EventKind::ControllerRejected
        ) {
            self.sender
                .try_send(event.clone())
                .expect("the two-submission fixture must not exhaust its event buffer");
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SuccessFixture {
    Embedded,
    EmbeddedRevision,
    Routed,
}

impl SuccessFixture {
    fn manifest(self, name: &str, slot: &str) -> TaskManifest {
        match self {
            Self::Embedded => embedded_manifest(name, slot),
            Self::EmbeddedRevision => embedded_revision(name, slot, 1),
            Self::Routed => routed_manifest(name, slot, 1, None),
        }
    }
}

#[test]
fn success_manifests_use_queue_admission() {
    for fixture in [
        SuccessFixture::Embedded,
        SuccessFixture::EmbeddedRevision,
        SuccessFixture::Routed,
    ] {
        assert_eq!(
            fixture
                .manifest("candidate", "shared-slot")
                .spec()
                .admission(),
            AdmissionPolicy::Queue,
            "{fixture:?} must wait for a previous controller owner to retire"
        );
    }
}

#[cfg(feature = "http")]
#[test]
fn http_success_manifest_uses_queue_admission() {
    assert_eq!(
        http_support::manifest("http-candidate", 0)
            .spec()
            .admission(),
        AdmissionPolicy::Queue,
        "HTTP success fixtures must wait for a previous controller owner to retire"
    );
}

async fn next_admission(receiver: &mut mpsc::Receiver<Event>) -> Event {
    bounded(receiver.recv())
        .await
        .expect("the admission subscriber must remain alive")
}

async fn success_fixtures_wait_for_a_busy_slot() {
    for fixture in [
        SuccessFixture::Embedded,
        SuccessFixture::EmbeddedRevision,
        SuccessFixture::Routed,
    ] {
        let runner = Arc::new(ControlledRunner::new("fixture-admission-runner"));
        let (sender, mut events) = mpsc::channel(8);
        let subscriber = Arc::new(AdmissionEvents { sender });
        let api = bounded(
            SupervisorApi::builder(router(runner.clone()))
                .with_subscribers(vec![subscriber])
                .start(),
        )
        .await
        .expect("fixture supervisor startup");

        let owner_started = Counter::new();
        let release_owner = Counter::new();
        let owner_canceled = Counter::new();
        let owner = bounded(api.create_embedded_task(
            embedded_manifest("owner", "shared-slot"),
            held_task(
                owner_started.clone(),
                release_owner.clone(),
                owner_canceled.clone(),
            ),
        ))
        .await
        .expect("held owner create");
        owner_started.wait(1).await;

        // Consume the owner's accepted event before submitting the candidate.
        // Taskvisor 0.9.0 exposes ControllerSubmitted, not ControllerQueued.
        let owner_admission = next_admission(&mut events).await;
        assert_eq!(owner_admission.kind, EventKind::ControllerSubmitted);
        assert_eq!(owner_admission.task.as_deref(), Some("shared-slot"));
        assert!(owner_admission.id.is_some());

        let candidate_started = match fixture {
            SuccessFixture::Routed => runner.task_starts.clone(),
            _ => Counter::new(),
        };
        let manifest = fixture.manifest("candidate", "shared-slot");
        let candidate = match fixture {
            SuccessFixture::Routed => bounded(api.create_task(manifest)).await,
            _ => {
                bounded(api.create_embedded_task(manifest, marked_task(candidate_started.clone())))
                    .await
            }
        }
        .expect("candidate desired-state commit");
        let candidate_admission = next_admission(&mut events).await;
        let started_while_owner_held = candidate_started.get();

        // Clean up even when the unfixed fixture produces ControllerRejected.
        // A terminal observation lets the red test report that typed rejection
        // immediately instead of spending the failure bound awaiting Succeeded.
        release_owner.increment();
        let owner_finished = wait_terminal(&api, owner.name()).await;
        let candidate_finished = wait_terminal(&api, candidate.name()).await;
        bounded(api.shutdown())
            .await
            .expect("fixture supervisor shutdown");

        assert_eq!(owner_finished.phase(), &TaskPhase::Succeeded);
        assert_eq!(owner_canceled.get(), 0);
        assert_eq!(candidate_admission.task.as_deref(), Some("shared-slot"));
        assert!(candidate_admission.id.is_some());
        assert_ne!(candidate_admission.id, owner_admission.id);
        assert_eq!(
            candidate_admission.kind,
            EventKind::ControllerSubmitted,
            "{fixture:?} must queue behind the held owner, not reject: {candidate_admission:?}"
        );
        assert_eq!(
            started_while_owner_held, 0,
            "{fixture:?} must not run before the queued slot is released"
        );
        assert_eq!(candidate_finished.phase(), &TaskPhase::Succeeded);
        assert_eq!(candidate_started.get(), 1);
    }
}

#[test]
fn success_fixtures_wait_for_a_busy_slot_current_thread() {
    current_thread().block_on(success_fixtures_wait_for_a_busy_slot());
}

#[test]
fn success_fixtures_wait_for_a_busy_slot_multi_thread() {
    multi_thread().block_on(success_fixtures_wait_for_a_busy_slot());
}

fn snapshot_runs(api: &SupervisorApi, task: &Task) -> Vec<TaskRun> {
    let base = TaskRunQuery::new().with_limit(4);
    let mut query = base.clone();
    let mut snapshot_version = None;
    let mut runs = Vec::new();
    loop {
        let page = api
            .query_task_runs(task.name(), &query)
            .expect("history snapshot page")
            .expect("history resource remains retained");
        match &snapshot_version {
            Some(version) => assert_eq!(&page.resource_version, version),
            None => snapshot_version = Some(page.resource_version),
        }
        runs.extend(page.items);
        match page.continuation {
            Some(continuation) => query = base.clone().with_continuation(continuation),
            None => return runs,
        }
    }
}

async fn same_slot_generations_preserve_every_run() {
    const GENERATIONS: u64 = 32;

    let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
        .await
        .expect("history supervisor startup");
    let first = retained_task(&api, embedded_revision("history", "history", 1)).await;
    let mut unexpected_terminal = None;
    for generation in 2..=GENERATIONS {
        let committed = bounded(api.apply_embedded_task(
            embedded_revision("history", "history", generation),
            immediate_task(),
        ))
        .await
        .expect("history generation apply");
        let finished = wait_task(&api, committed.name(), |task| {
            task.metadata().generation() == generation && task.phase().is_terminal()
        })
        .await;
        bounded(api.cancel_task(committed.name()))
            .await
            .expect("history generation settlement");
        if finished.phase() != &TaskPhase::Succeeded {
            unexpected_terminal = Some(finished);
            break;
        }
    }
    let runs = snapshot_runs(&api, &first);
    bounded(api.shutdown())
        .await
        .expect("history supervisor shutdown");

    assert!(
        unexpected_terminal.is_none(),
        "same-slot fixture failed before all generations ran: {unexpected_terminal:?}"
    );
    assert_eq!(runs.len(), GENERATIONS as usize);
    assert!(runs.iter().map(TaskRun::generation).eq(1..=GENERATIONS));
    assert!(runs.iter().all(|run| !run.is_active()));
}

#[test]
fn same_slot_generations_preserve_every_run_current_thread() {
    current_thread().block_on(same_slot_generations_preserve_every_run());
}

#[test]
fn same_slot_generations_preserve_every_run_multi_thread() {
    multi_thread().block_on(same_slot_generations_preserve_every_run());
}
