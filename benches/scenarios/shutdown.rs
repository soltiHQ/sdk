//! Complete SDK shutdown with running attempts, blocked builds, watches, and state callbacks.

#[path = "support/sinks.rs"]
mod sinks;

use std::{
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::{
    fixtures::{RUNTIMES, bounded, embedded_manifest},
    report::{CaseFamily, print_suite_header, record_case},
};
use solti_core::SupervisorApi;
use solti_model::{
    ExtensionWorkload, Labels, RestartPolicy, Task, TaskFilter, TaskManifest, TaskSpec,
    TaskWorkload, WorkloadTypeMeta,
};
use solti_runner::{
    BuildCancellation, BuildContext, BuildScope, RunId, Runner, RunnerError, RunnerRouter,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio_stream::StreamExt;

use sinks::{Progress, ReleaseOnDrop, StateSink, drain_state};

const EMPTY: CaseFamily = CaseFamily::lifecycle(
    "shutdown/empty",
    "EMPTY SDK SHUTDOWN",
    "shutdown",
    "shutdowns",
    "SupervisorApi::shutdown call through successful completion of SDK-owned worker drain",
    "Tokio runtime and supervisor construction",
)
.without_lifecycle_interpretation();

const ACTIVE: CaseFamily = CaseFamily::lifecycle(
    "shutdown/active",
    "SHUTDOWN WITH COOPERATIVE TASKS",
    "shutdown",
    "shutdowns",
    "shutdown call through stopped task bodies and SDK-owned worker drain",
    "runtime, supervisor, task setup and body-start readiness; post-return assertions",
)
.without_lifecycle_interpretation();

const MIXED: CaseFamily = CaseFamily::lifecycle(
    "shutdown/mixed", "SHUTDOWN WITH BUILDS AND BLOCKED STATE CALLBACKS", "shutdown", "shutdowns",
    "shutdown call, first pending poll, callback release, and complete SDK-owned worker drain",
    "runtime, running tasks, four blocked builds, initial watch, state-queue setup and post-return assertions",
).without_lifecycle_interpretation();

struct BlockedBuilds {
    entered: Arc<Progress>,
    finished: Arc<Progress>,
}

struct BuildFinished(Arc<Progress>);

impl Drop for BuildFinished {
    fn drop(&mut self) {
        self.0.advance();
    }
}

#[solti_runner::async_trait]
impl Runner for BlockedBuilds {
    fn name(&self) -> &str {
        "shutdown-blocked-build"
    }
    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new("bench.example.org/v1", "BlockedBuild").unwrap()]
    }
    async fn build_task(
        &self,
        _: &Task,
        _: &RunId,
        _: &BuildContext,
        cancellation: &BuildCancellation,
        _: &mut BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let _finished = BuildFinished(self.finished.clone());
        self.entered.advance();
        cancellation.cancelled().await;
        Err(RunnerError::BuildCancelled)
    }
}

fn blocked_manifest(index: usize) -> TaskManifest {
    let name = format!("shutdown-build-{index}");
    let workload = TaskWorkload::Extension(
        ExtensionWorkload::new(
            "bench.example.org/v1",
            "BlockedBuild",
            serde_json::json!({}),
        )
        .unwrap(),
    );
    let spec = TaskSpec::builder(&name, workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()
        .unwrap();
    TaskManifest::new(&name, spec).unwrap()
}

async fn run(active: usize, mixed: bool) -> Duration {
    let started = Arc::new(Progress::default());
    let stopped = Arc::new(Progress::default());
    let builds = Arc::new(BlockedBuilds {
        entered: Arc::new(Progress::default()),
        finished: Arc::new(Progress::default()),
    });
    let sink = StateSink::new(512);
    let _release = ReleaseOnDrop(sink.gate.clone());
    let mut router = RunnerRouter::new();
    router.register(builds.clone()).unwrap();
    let builder = SupervisorApi::builder(router);
    let builder = if mixed {
        builder.with_state_sink(sink.clone())
    } else {
        builder
    };
    let api = builder.start().await.unwrap();
    for index in 0..active {
        let name = format!("shutdown-active-{index}");
        let (started, stopped) = (started.clone(), stopped.clone());
        bounded(api.create_embedded_task(
            embedded_manifest(&name, &name),
            TaskFn::arc(move |ctx: TaskContext| {
                let (started, stopped) = (started.clone(), stopped.clone());
                async move {
                    started.advance();
                    ctx.cancelled().await;
                    stopped.advance();
                    Err(TaskError::Canceled)
                }
            }),
        ))
        .await
        .unwrap();
    }
    started.wait(active).await;
    let mut watch = api.watch_tasks(&TaskFilter::new(), None).unwrap();
    if mixed {
        assert!(active > 0);
        for index in 0..4 {
            bounded(api.create_task(blocked_manifest(index)))
                .await
                .unwrap();
        }
        builds.entered.wait(4).await;
        drain_state(&api).await;
        let before = sink.entered.get();
        sink.gate.pause();
        let mut labels = Labels::new();
        labels.insert("shutdown-fixture", "queued");
        let metadata = embedded_manifest("shutdown-active-0", "shutdown-active-0")
            .with_labels(labels)
            .unwrap();
        bounded(api.apply_embedded_task(metadata, TaskFn::arc(|_: TaskContext| async { Ok(()) })))
            .await
            .unwrap();
        sink.entered.wait(before + 1).await;
    }
    let start = Instant::now();
    if mixed {
        let shutdown = api.shutdown();
        tokio::pin!(shutdown);
        std::future::poll_fn(|cx| {
            assert!(
                shutdown.as_mut().poll(cx).is_pending(),
                "shutdown must wait for the blocked callback"
            );
            Poll::Ready(())
        })
        .await;
        sink.gate.release();
        bounded(shutdown).await.unwrap();
    } else {
        bounded(api.shutdown()).await.unwrap();
    }
    let elapsed = start.elapsed();
    assert_eq!(stopped.get(), active);
    if mixed {
        assert_eq!(builds.finished.get(), 4);
        let status = api.state_persistence_status().unwrap();
        assert!(status.healthy());
        assert_eq!(status.queued(), 0);
    }
    bounded(async {
        while let Some(event) = watch.next().await {
            event.expect("retained watch must not expire");
        }
    })
    .await;
    elapsed
}

fn benchmarks(c: &mut Criterion) {
    print_suite_header("shutdown");
    for (family, mixed, counts) in [
        (EMPTY, false, &[0_usize][..]),
        (ACTIVE, false, &[8_usize, 32][..]),
        (MIXED, true, &[8_usize, 32][..]),
    ] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for (runtime_name, runtime) in RUNTIMES {
            for &active in counts {
                let parameter = format!("{active}_active_tasks");
                group.bench_with_input(
                    BenchmarkId::new(runtime_name, &parameter),
                    &active,
                    |b, &active| {
                        record_case(family, runtime_name, Some(parameter.clone()));
                        let runtime = runtime();
                        b.iter_custom(|iterations| {
                            runtime.block_on(async {
                                let mut total = Duration::ZERO;
                                for _ in 0..iterations {
                                    total += run(active, mixed).await;
                                }
                                total
                            })
                        });
                    },
                );
            }
        }
        group.finish();
    }
}

criterion_group!(benches, benchmarks);

fn main() {
    solti_benches::report::benchmark_main("shutdown", benches);
}
