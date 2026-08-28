//! Committed-state delivery, backpressure recovery, and best-effort output hooks.
//!
//! The sinks are controlled in-process callbacks, not durable storage adapters.

#[path = "support/sinks.rs"]
mod sinks;

use std::{
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::{
    fixtures::{RUNTIMES, bounded, embedded_manifest, wait_terminal},
    report::{CaseFamily, print_suite_header, record_case},
};
use solti_core::{PersistenceConfig, SupervisorApi};
use solti_model::{
    ExtensionWorkload, Labels, RestartPolicy, Task, TaskManifest, TaskSpec, TaskWorkload,
    WorkloadTypeMeta,
};
use solti_runner::{
    BuildCancellation, BuildContext, BuildScope, RunId, Runner, RunnerError, RunnerRouter,
    request_output_sink,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::sync::Semaphore;

use sinks::{Progress, ReleaseOnDrop, StateSink, drain_state};

const DELIVERY: CaseFamily = CaseFamily::lifecycle(
    "persistence/state/commit_and_deliver",
    "STATE COMMITS AND CALLBACK DELIVERY",
    "delivered state update",
    "delivered state updates",
    "metadata apply calls through delivery of every corresponding state callback",
    "runtime, supervisor, settled Task, prepared manifests, and shutdown",
)
.without_lifecycle_interpretation();

const RECOVERY: CaseFamily = CaseFamily::lifecycle(
    "persistence/state/backpressure_recovery", "SATURATED STATE QUEUE RECOVERY",
    "recovery cycle", "recovery cycles",
    "release of a blocked sink through admission and delivery of the waiting metadata update and queue drain",
    "runtime, settled Task, filling the queue, proving the next write is pending, and shutdown",
).without_lifecycle_interpretation();

const OUTPUT: CaseFamily = CaseFamily::intake(
    "persistence/output/blocked_sink",
    "OUTPUT PUBLICATION WITH A BLOCKED CALLBACK",
    "published chunk",
    "published chunks",
    "release of a ready producer through its final chunk publication; callback copies may be dropped",
    "runtime, supervisor, runner build/start, first blocked callback, callback drain, and shutdown",
);

fn immediate() -> TaskRef {
    TaskFn::arc(|_: TaskContext| async { Ok(()) })
}

fn updated(base: &TaskManifest, revision: usize) -> TaskManifest {
    let mut labels = Labels::new();
    labels.insert("revision", revision.to_string());
    base.clone().with_labels(labels).expect("benchmark labels")
}

async fn state_fixture(
    work: usize,
    capacity: usize,
) -> (SupervisorApi, Arc<StateSink>, TaskManifest) {
    let sink = StateSink::new(work);
    let api = SupervisorApi::builder(RunnerRouter::new())
        .with_state_sink(sink.clone())
        .with_persistence_config(
            PersistenceConfig::new()
                .try_with_state_queue_capacity(capacity)
                .unwrap(),
        )
        .start()
        .await
        .unwrap();
    let base = embedded_manifest("state-hook", "state-hook");
    api.create_embedded_task(base.clone(), immediate())
        .await
        .unwrap();
    wait_terminal(&api, base.name()).await;
    bounded(api.cancel_task(base.name())).await.unwrap();
    drain_state(&api).await;
    (api, sink, base)
}

async fn delivery(work: usize) -> Duration {
    let (api, sink, base) = state_fixture(work, 32).await;
    let updates: Vec<_> = (0..16).map(|revision| updated(&base, revision)).collect();
    let first = sink.delivered.get();
    let task = immediate();
    let start = Instant::now();
    for manifest in updates {
        bounded(api.apply_embedded_task(manifest, Arc::clone(&task)))
            .await
            .unwrap();
    }
    sink.delivered.wait(first + 16).await;
    drain_state(&api).await;
    let elapsed = start.elapsed();
    assert_eq!(sink.delivered.get() - first, 16);
    bounded(api.shutdown()).await.unwrap();
    elapsed
}

async fn recovery() -> Duration {
    let (api, sink, base) = state_fixture(512, 2).await;
    let _release = ReleaseOnDrop(Arc::clone(&sink.gate));
    let entered = sink.entered.get();
    let delivered = sink.delivered.get();
    let capacity = api.state_persistence_status().unwrap().capacity();
    sink.gate.pause();
    bounded(api.apply_embedded_task(updated(&base, 0), immediate()))
        .await
        .unwrap();
    sink.entered.wait(entered + 1).await;
    for revision in 1..capacity {
        bounded(api.apply_embedded_task(updated(&base, revision), immediate()))
            .await
            .unwrap();
    }
    assert_eq!(api.state_persistence_status().unwrap().queued(), capacity);
    let waiting = api.apply_embedded_task(updated(&base, capacity), immediate());
    tokio::pin!(waiting);
    std::future::poll_fn(|cx| {
        assert!(
            waiting.as_mut().poll(cx).is_pending(),
            "saturated state write did not wait"
        );
        Poll::Ready(())
    })
    .await;
    let start = Instant::now();
    sink.gate.release();
    bounded(waiting).await.unwrap();
    sink.delivered.wait(delivered + capacity + 1).await;
    drain_state(&api).await;
    let elapsed = start.elapsed();
    assert_eq!(sink.delivered.get() - delivered, capacity + 1);
    bounded(api.shutdown()).await.unwrap();
    elapsed
}

struct Emitter {
    release: Arc<Semaphore>,
    started: Arc<Progress>,
    published: Arc<Progress>,
    chunks: usize,
}

#[solti_runner::async_trait]
impl Runner for Emitter {
    fn name(&self) -> &str {
        "persistence-emitter"
    }
    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new("bench.example.org/v1", "Output").unwrap()]
    }
    async fn build_task(
        &self,
        task: &Task,
        _: &RunId,
        context: &BuildContext,
        _: &BuildCancellation,
        _: &mut BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let name = task.name().clone();
        let generation = task.metadata().generation();
        let publisher = Arc::clone(context.output_publisher());
        let release = Arc::clone(&self.release);
        let started = Arc::clone(&self.started);
        let published = Arc::clone(&self.published);
        let chunks = self.chunks;
        Ok(TaskFn::arc(move |_: TaskContext| {
            let (name, publisher, release, started, published) = (
                name.clone(),
                publisher.clone(),
                release.clone(),
                started.clone(),
                published.clone(),
            );
            async move {
                let sink = request_output_sink(&publisher, &name, generation, 1)
                    .expect("core output publisher");
                sink.stdout_line_bytes(b"untimed callback readiness marker");
                started.advance();
                release.acquire().await.unwrap().forget();
                for _ in 0..chunks {
                    sink.stdout_line_bytes(b"controlled output payload");
                }
                published.advance();
                Ok::<(), TaskError>(())
            }
        }))
    }
}

async fn output(chunks: usize) -> Duration {
    let sink = sinks::OutputSink::paused();
    let _release = ReleaseOnDrop(Arc::clone(&sink.gate));
    let producer = Arc::new(Emitter {
        release: Arc::new(Semaphore::new(0)),
        started: Arc::new(Progress::default()),
        published: Arc::new(Progress::default()),
        chunks,
    });
    let mut router = RunnerRouter::new();
    router.register(producer.clone()).unwrap();
    let api = SupervisorApi::builder(router)
        .with_output_sink(sink.clone())
        .with_persistence_config(
            PersistenceConfig::new()
                .try_with_output_queue_capacity(8)
                .unwrap(),
        )
        .start()
        .await
        .unwrap();
    let workload = TaskWorkload::Extension(
        ExtensionWorkload::new("bench.example.org/v1", "Output", serde_json::json!({})).unwrap(),
    );
    let spec = TaskSpec::builder("output-hook", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()
        .unwrap();
    let manifest = TaskManifest::new("output-hook", spec).unwrap();
    let name = manifest.name().clone();
    bounded(api.create_task(manifest)).await.unwrap();
    producer.started.wait(1).await;
    sink.entered.wait(1).await;
    let start = Instant::now();
    producer.release.add_permits(1);
    producer.published.wait(1).await;
    let elapsed = start.elapsed();
    assert!(api.output_persistence_status().unwrap().dropped() > 0);
    sink.gate.release();
    wait_terminal(&api, &name).await;
    bounded(api.shutdown()).await.unwrap();
    let status = api.output_persistence_status().unwrap();
    assert!(status.healthy());
    assert_eq!(status.queued(), 0);
    elapsed
}

fn benchmarks(c: &mut Criterion) {
    print_suite_header("persistence");
    let mut group = c.benchmark_group(DELIVERY.group_id);
    group.throughput(Throughput::Elements(16));
    for (runtime_name, runtime) in RUNTIMES {
        for work in [0, 512] {
            let parameter = format!("16_updates_{work}_callback_iterations");
            group.bench_with_input(
                BenchmarkId::new(runtime_name, &parameter),
                &work,
                |b, &work| {
                    record_case(DELIVERY, runtime_name, Some(parameter.clone()));
                    let runtime = runtime();
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                total += delivery(work).await;
                            }
                            total
                        })
                    });
                },
            );
        }
    }
    group.finish();
    let mut group = c.benchmark_group(RECOVERY.group_id);
    group.throughput(Throughput::Elements(1));
    for (runtime_name, runtime) in RUNTIMES {
        group.bench_function(runtime_name, |b| {
            record_case(RECOVERY, runtime_name, None);
            let runtime = runtime();
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iterations {
                        total += recovery().await;
                    }
                    total
                })
            });
        });
    }
    group.finish();
    let mut group = c.benchmark_group(OUTPUT.group_id);
    for (runtime_name, runtime) in RUNTIMES {
        for chunks in [64, 256] {
            group.throughput(Throughput::Elements(chunks as u64));
            let parameter = format!("{chunks}_chunks_8_callback_slots");
            group.bench_with_input(
                BenchmarkId::new(runtime_name, &parameter),
                &chunks,
                |b, &chunks| {
                    record_case(OUTPUT, runtime_name, Some(parameter.clone()));
                    let runtime = runtime();
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                total += output(chunks).await;
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

criterion_group!(benches, benchmarks);

fn main() {
    solti_benches::report::benchmark_main("persistence", benches);
}
