//! Controlled workloads shared only by the project-level core scenarios.

#![allow(dead_code)]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use solti_benches::fixtures::{bounded, embedded_manifest, wait_task};
use solti_core::SupervisorApi;
use solti_model::{
    AdmissionPolicy, ConditionStatus, EmbeddedSpec, ExtensionWorkload, Labels, RestartPolicy, Task,
    TaskId, TaskManifest, TaskPhase, TaskSpec, TaskWorkload, WorkloadTypeMeta,
};
use solti_runner::{
    BuildCancellation, BuildContext, BuildScope, RunId, Runner, RunnerError, RunnerRouter,
};
use taskvisor::{
    Event, EventKind, RejectionKind, Subscribe, TaskContext, TaskError, TaskFn, TaskRef,
};
use tokio::sync::{Notify, Semaphore};

pub const BENCH_API: &str = "bench.example.org/v1";
pub const BENCH_KIND: &str = "Controlled";

/// A persistent notification: readiness is never represented by a sleep.
#[derive(Default)]
pub struct Counter {
    value: AtomicUsize,
    changed: Notify,
}

impl Counter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn get(&self) -> usize {
        self.value.load(Ordering::Acquire)
    }

    pub fn increment(&self) -> usize {
        let next = self.value.fetch_add(1, Ordering::AcqRel) + 1;
        self.changed.notify_waiters();
        next
    }

    pub async fn wait(&self, minimum: usize) {
        bounded(async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.get() >= minimum {
                    return;
                }
                changed.await;
            }
        })
        .await;
    }
}

pub fn immediate_task() -> TaskRef {
    TaskFn::arc(|_ctx: TaskContext| async { Ok::<(), TaskError>(()) })
}

pub fn marked_task(started: Arc<Counter>) -> TaskRef {
    TaskFn::arc(move |_ctx: TaskContext| {
        let started = Arc::clone(&started);
        async move {
            started.increment();
            Ok::<(), TaskError>(())
        }
    })
}

pub fn held_task(started: Arc<Counter>, release: Arc<Counter>, canceled: Arc<Counter>) -> TaskRef {
    TaskFn::arc(move |ctx: TaskContext| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let canceled = Arc::clone(&canceled);
        async move {
            started.increment();
            tokio::select! {
                _ = release.wait(1) => Ok(()),
                _ = ctx.cancelled() => {
                    canceled.increment();
                    Err(TaskError::Canceled)
                },
            }
        }
    })
}

pub fn embedded_revision(name: &str, slot: &str, revision: u64) -> TaskManifest {
    let base = embedded_manifest(name, slot);
    let spec = base.spec().clone().with_workload(TaskWorkload::Embedded(
        EmbeddedSpec::new(format!("revision-{revision}")).expect("valid benchmark revision"),
    ));
    TaskManifest::new(name, spec).expect("valid embedded benchmark manifest")
}

/// A queued manifest for controlled routed workloads.
pub fn routed_manifest(
    name: &str,
    slot: &str,
    revision: u64,
    runner: Option<&str>,
) -> TaskManifest {
    let workload = TaskWorkload::Extension(
        ExtensionWorkload::new(
            BENCH_API,
            BENCH_KIND,
            serde_json::json!({"revision": revision}),
        )
        .expect("valid benchmark extension"),
    );
    let mut spec = TaskSpec::builder(slot, workload, 30_000_u64)
        .admission(AdmissionPolicy::Queue)
        .restart(RestartPolicy::Never);
    if let Some(runner) = runner {
        spec = spec.runner_selector(format!("backend={runner}").parse().expect("valid selector"));
    }
    TaskManifest::new(name, spec.build().expect("valid benchmark spec"))
        .expect("valid benchmark manifest")
}

pub fn with_label(manifest: TaskManifest, value: &str) -> TaskManifest {
    let mut labels = Labels::new();
    labels.insert("revision", value);
    manifest
        .with_labels(labels)
        .expect("valid benchmark labels")
}

pub async fn retained_task(api: &SupervisorApi, manifest: TaskManifest) -> Task {
    let task = bounded(api.create_embedded_task(manifest, immediate_task()))
        .await
        .expect("fixture create failed");
    wait_task(api, task.name(), |task| {
        task.phase() == &TaskPhase::Succeeded
    })
    .await;
    // Public cancellation settles completion and its state projection before a
    // fixture is used for metadata-only or collection measurements. It does not
    // prove controller Idle; subsequent success fixtures explicitly use Queue.
    bounded(api.cancel_task(task.name()))
        .await
        .expect("fixture settlement failed");
    api.get_task(task.name())
        .expect("fixture must remain retained")
}

pub async fn observed(api: &SupervisorApi, name: &TaskId, generation: u64) -> Task {
    wait_task(api, name, |task| {
        task.metadata().generation() == generation
            && task.status().observed_generation() == generation
            && task.status().reconciled().status() == ConditionStatus::True
    })
    .await
}

/// A real registered extension runner with explicitly controlled build work.
pub struct ControlledRunner {
    pub name: &'static str,
    pub builds: Arc<Counter>,
    pub task_starts: Arc<Counter>,
    pub active: Arc<AtomicUsize>,
    pub peak: Arc<AtomicUsize>,
    pub permits: Option<Arc<Semaphore>>,
    pub block_before_generation: Option<u64>,
    pub fail_first: bool,
}

impl ControlledRunner {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            builds: Counter::new(),
            task_starts: Counter::new(),
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            permits: None,
            block_before_generation: None,
            fail_first: false,
        }
    }
}

struct ActiveBuild(Arc<AtomicUsize>);

impl Drop for ActiveBuild {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[solti_runner::async_trait]
impl Runner for ControlledRunner {
    fn name(&self) -> &str {
        self.name
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new(BENCH_API, BENCH_KIND).expect("valid benchmark GVK")]
    }

    async fn build_task(
        &self,
        task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        cancellation: &BuildCancellation,
        _scope: &mut BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        let _active = ActiveBuild(Arc::clone(&self.active));
        let build = self.builds.increment();
        if self.fail_first && build == 1 {
            return Err(RunnerError::Internal(
                "controlled first-build failure".into(),
            ));
        }
        if self
            .block_before_generation
            .is_some_and(|generation| task.metadata().generation() < generation)
        {
            cancellation.cancelled().await;
            return Err(RunnerError::BuildCancelled);
        }
        if let Some(permits) = &self.permits {
            tokio::select! {
                _ = cancellation.cancelled() => return Err(RunnerError::BuildCancelled),
                permit = permits.acquire() => permit.expect("benchmark gate remains open").forget(),
            }
        }
        Ok(marked_task(Arc::clone(&self.task_starts)))
    }
}

pub fn router(runner: Arc<ControlledRunner>) -> RunnerRouter {
    let mut router = RunnerRouter::new();
    router
        .register(runner)
        .expect("benchmark runner registration");
    router
}

pub fn labeled_router(runners: &[Arc<ControlledRunner>]) -> RunnerRouter {
    let mut router = RunnerRouter::new();
    for runner in runners {
        let mut labels = Labels::new();
        labels.insert("backend", runner.name);
        router
            .register_with_labels(Arc::clone(runner) as Arc<dyn Runner>, labels)
            .expect("benchmark runner registration");
    }
    router
}

/// Counts the actual typed busy-slot decisions, not only terminal phases.
#[derive(Default)]
pub struct BusyRejections {
    pub count: Counter,
}

impl Subscribe for BusyRejections {
    fn on_event(&self, event: &Event) {
        if event.kind == EventKind::ControllerRejected
            && event.rejection_kind == Some(RejectionKind::SlotBusy)
        {
            self.count.increment();
        }
    }

    fn name(&self) -> &'static str {
        "bench-busy-rejections"
    }

    fn queue_capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(2048).expect("positive event capacity")
    }
}
