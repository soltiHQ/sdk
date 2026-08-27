//! Container lifecycle benchmarks: controlled engine and explicit real-containerd lane.

use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::{
    fixtures::{RUNTIMES, WAIT_BOUND},
    report::{CaseFamily, benchmark_main, print_suite_header, record_case},
};
use solti_exec::container::{
    ContainerAttempt, ContainerEngine, ContainerEngineBinding, ContainerEngineError,
    ContainerEngineInfo, ContainerExitStatus, ContainerOutput, ContainerRequest,
    register_container_runner,
};
use solti_model::{
    ContainerSpec, OutputEvent, Task, TaskEnv, TaskId, TaskManifest, TaskSpec, TaskWorkload,
};
use solti_runner::{OutputPublisher, OutputSink, RunnerRouter};
use taskvisor::TaskContext;

const CONTROLLED: CaseFamily = CaseFamily::lifecycle(
    "container/controlled/lifecycle", "CONTAINER · CONTROLLED ENGINE", "controlled attempt", "controlled attempts",
    "built Container task spawn through controlled engine create/start/wait, output drain, cleanup and attempt drop",
    "runtime, router, engine and task build; assertions; this in-memory engine creates no real container",
).without_lifecycle_interpretation();
const STOP: CaseFamily = CaseFamily::lifecycle(
    "container/controlled/stop", "CONTAINER · CONTROLLED STOP", "stopped controlled attempt", "stopped controlled attempts",
    "cancel or force-drop of a ready controlled engine wait through logical completion and verified synchronous ownership release",
    "runtime, API, engine, build and startup; assertions and API shutdown; no remote container or daemon",
).without_lifecycle_interpretation();
#[cfg(feature = "containerd")]
const REAL: CaseFamily = CaseFamily::lifecycle(
    "container/containerd/lifecycle", "CONTAINERD · REAL ATTEMPT", "completed container attempt", "completed container attempts",
    "built task spawn through image resolution/pull/unpack, containerd create/start/wait, output drain and cleanup",
    "runtime, environment preparation, connect/probe, router and task build; marker assertion and terminal engine shutdown; not cache-only",
).without_lifecycle_interpretation();

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Success,
    StartFailure,
    Nonzero,
    Wait,
}

#[derive(Default)]
struct Counts {
    created: AtomicUsize,
    started: AtomicUsize,
    terminated: AtomicUsize,
    cleaned: AtomicUsize,
    dropped: AtomicUsize,
    active: AtomicUsize,
    waiting: tokio::sync::Notify,
}

struct ControlledEngine {
    behavior: Behavior,
    counts: Arc<Counts>,
}

#[solti_runner::async_trait]
impl ContainerEngine for ControlledEngine {
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError> {
        Ok(ContainerEngineInfo::new(
            "controlled-in-memory-bench-engine",
            "1",
        ))
    }
    async fn create_attempt(
        &self,
        _: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError> {
        self.counts.created.fetch_add(1, Ordering::Relaxed);
        self.counts.active.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(ControlledAttempt {
            behavior: self.behavior,
            counts: Arc::clone(&self.counts),
            terminated: false,
            stdout: Some(Box::pin(Cursor::new(b"solti-bench-container\n".to_vec()))),
            stderr: Some(Box::pin(Cursor::new(b"controlled-diagnostic\n".to_vec()))),
        }))
    }
}

struct ControlledAttempt {
    behavior: Behavior,
    counts: Arc<Counts>,
    terminated: bool,
    stdout: Option<ContainerOutput>,
    stderr: Option<ContainerOutput>,
}

#[solti_runner::async_trait]
impl ContainerAttempt for ControlledAttempt {
    fn take_stdout(&mut self) -> Option<ContainerOutput> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<ContainerOutput> {
        self.stderr.take()
    }
    async fn start(&mut self) -> Result<(), ContainerEngineError> {
        self.counts.started.fetch_add(1, Ordering::Relaxed);
        if self.behavior == Behavior::StartFailure {
            Err(ContainerEngineError::retryable("controlled start failure"))
        } else {
            Ok(())
        }
    }
    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError> {
        if self.behavior == Behavior::Wait && !self.terminated {
            self.counts.waiting.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ContainerExitStatus::new(
            if self.behavior == Behavior::Nonzero {
                17
            } else {
                0
            },
        ))
    }
    async fn terminate(&mut self) -> Result<(), ContainerEngineError> {
        if !self.terminated {
            self.counts.terminated.fetch_add(1, Ordering::Relaxed);
            self.terminated = true;
        }
        Ok(())
    }
    async fn cleanup(&mut self) -> Result<(), ContainerEngineError> {
        self.counts.cleaned.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for ControlledAttempt {
    fn drop(&mut self) {
        self.counts.active.fetch_sub(1, Ordering::Relaxed);
        self.counts.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Markers(Arc<AtomicUsize>);

impl OutputPublisher for Markers {
    fn sink_for(&self, _: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        let markers = Arc::clone(&self.0);
        Some(OutputSink::new(generation, attempt, move |event| {
            if let OutputEvent::Chunk(chunk) = event
                && chunk.line.as_ref() == b"solti-bench-container"
            {
                markers.fetch_add(1, Ordering::Relaxed);
            }
        }))
    }
}

fn workload(image: &str) -> TaskWorkload {
    TaskWorkload::Container(ContainerSpec::new(
        image.to_owned(),
        Some(vec!["/bin/sh".to_owned()]),
        vec![
            "-c".to_owned(),
            "printf '%s\\n' solti-bench-container".to_owned(),
        ],
        TaskEnv::new(),
    ))
}

fn spec(image: &str) -> TaskSpec {
    TaskSpec::builder("container-slot", workload(image), 20_000_u64)
        .build()
        .unwrap()
}

fn fixture(behavior: Behavior) -> (RunnerRouter, Arc<Counts>, Arc<Markers>) {
    let counts = Arc::new(Counts::default());
    let engine: Arc<dyn ContainerEngine> = Arc::new(ControlledEngine {
        behavior,
        counts: Arc::clone(&counts),
    });
    let output = Arc::new(Markers::default());
    let mut router = RunnerRouter::new().with_output_publisher(output.clone());
    register_container_runner(
        &mut router,
        "controlled-container",
        ContainerEngineBinding::drop_releases(engine),
    )
    .unwrap();
    (router, counts, output)
}

fn bench_controlled(c: &mut Criterion) {
    print_suite_header("container");
    let mut group = c.benchmark_group(CONTROLLED.group_id);
    group.throughput(Throughput::Elements(1));
    for &(rt_name, rt_fn) in &RUNTIMES {
        for (label, behavior) in [
            ("success", Behavior::Success),
            ("start_failure", Behavior::StartFailure),
            ("nonzero_exit", Behavior::Nonzero),
        ] {
            group.bench_function(BenchmarkId::new(rt_name, label), |b| {
                record_case(CONTROLLED, rt_name, Some(label.to_owned()));
                let rt = rt_fn();
                let (router, counts, output) = fixture(behavior);
                let built = rt
                    .block_on(router.build(
                        &Task::new("controlled-container", spec("controlled:fixture")).unwrap(),
                    ))
                    .unwrap();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let expected = counts.created.load(Ordering::Relaxed) + 1;
                            let start = Instant::now();
                            let result = tokio::time::timeout(
                                WAIT_BOUND,
                                built.task().spawn(TaskContext::detached()),
                            )
                            .await
                            .expect("controlled lifecycle deadline");
                            total += start.elapsed();
                            if behavior == Behavior::Success {
                                result.expect("controlled success");
                            } else {
                                assert!(matches!(result, Err(taskvisor::TaskError::Fail { .. })));
                            }
                            assert_eq!(counts.created.load(Ordering::Relaxed), expected);
                            assert_eq!(counts.cleaned.load(Ordering::Relaxed), expected);
                            assert_eq!(counts.dropped.load(Ordering::Relaxed), expected);
                            assert_eq!(counts.active.load(Ordering::Relaxed), 0);
                            assert_eq!(output.0.load(Ordering::Relaxed), expected);
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

fn bench_stop(c: &mut Criterion) {
    let mut group = c.benchmark_group(STOP.group_id);
    group.throughput(Throughput::Elements(1));
    for &(rt_name, rt_fn) in &RUNTIMES {
        for action in ["cooperative_cancel", "force_drop"] {
            group.bench_function(BenchmarkId::new(rt_name, action), |b| {
                record_case(STOP, rt_name, Some(action.to_owned()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let (router, counts, _output) = fixture(Behavior::Wait);
                            if action == "force_drop" {
                                let built = router
                                    .build(
                                        &Task::new("container-stop", spec("controlled:fixture"))
                                            .unwrap(),
                                    )
                                    .await
                                    .unwrap();
                                let task = built.into_task();
                                let attempt = tokio::spawn(async move {
                                    task.spawn(TaskContext::detached()).await
                                });
                                tokio::time::timeout(WAIT_BOUND, counts.waiting.notified())
                                    .await
                                    .expect("controlled wait readiness");
                                let start = Instant::now();
                                attempt.abort();
                                assert!(attempt.await.expect_err("aborted attempt").is_cancelled());
                                assert_eq!(counts.active.load(Ordering::Relaxed), 0);
                                total += start.elapsed();
                            } else {
                                let api = solti_core::SupervisorApi::builder(router)
                                    .start()
                                    .await
                                    .unwrap();
                                let task = api
                                    .create_task(
                                        TaskManifest::new(
                                            "container-stop",
                                            spec("controlled:fixture"),
                                        )
                                        .unwrap(),
                                    )
                                    .await
                                    .unwrap();
                                tokio::time::timeout(WAIT_BOUND, counts.waiting.notified())
                                    .await
                                    .expect("controlled wait readiness");
                                let start = Instant::now();
                                api.cancel_task(task.name())
                                    .await
                                    .expect("cancel controlled container");
                                tokio::time::timeout(WAIT_BOUND, async {
                                    while !api.get_task(task.name()).unwrap().phase().is_terminal()
                                    {
                                        tokio::task::yield_now().await;
                                    }
                                })
                                .await
                                .expect("canceled container terminal state");
                                assert_eq!(counts.active.load(Ordering::Relaxed), 0);
                                total += start.elapsed();
                                assert_eq!(counts.terminated.load(Ordering::Relaxed), 1);
                                assert_eq!(counts.cleaned.load(Ordering::Relaxed), 1);
                                api.shutdown().await.expect("controlled API shutdown");
                            }
                            assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

#[cfg(feature = "containerd")]
fn bench_containerd(c: &mut Criterion) {
    use solti_exec::container::containerd::{ContainerNetwork, ContainerdConfig, ContainerdEngine};
    if std::env::var("SOLTI_BENCH_CONTAINERD").as_deref() != Ok("1") {
        eprintln!(
            "containerd benchmark skipped: requires SOLTI_BENCH_CONTAINERD=1 and explicitly provisioned Linux daemon/image"
        );
        return;
    }
    assert!(
        cfg!(target_os = "linux"),
        "real containerd benchmarks require Linux"
    );
    let required = |name: &str| {
        std::env::var(name)
            .unwrap_or_else(|_| panic!("set {name} for the provisioned containerd benchmark"))
    };
    let image = required("SOLTI_BENCH_CONTAINERD_IMAGE");
    let socket = required("SOLTI_BENCH_CONTAINERD_SOCKET");
    let namespace = required("SOLTI_BENCH_CONTAINERD_NAMESPACE");
    let io_root = required("SOLTI_BENCH_CONTAINERD_IO_ROOT");
    let snapshotter = required("SOLTI_BENCH_CONTAINERD_SNAPSHOTTER");
    let runtime = required("SOLTI_BENCH_CONTAINERD_RUNTIME");
    let mut group = c.benchmark_group(REAL.group_id);
    group.throughput(Throughput::Elements(1));
    for &(rt_name, rt_fn) in &RUNTIMES {
        group.bench_function(rt_name, |b| {
            record_case(REAL, rt_name, None);
            let rt = rt_fn();
            let config = ContainerdConfig::new(&socket, &namespace, &snapshotter, &runtime)
                .with_network(ContainerNetwork::Host)
                .with_io_root(&io_root);
            let engine = Arc::new(
                rt.block_on(ContainerdEngine::connect(config))
                    .expect("connect provisioned containerd"),
            );
            rt.block_on(engine.probe())
                .expect("probe provisioned containerd");
            let output = Arc::new(Markers::default());
            let mut router = RunnerRouter::new().with_output_publisher(output.clone());
            register_container_runner(&mut router, "real-containerd", Arc::clone(&engine)).unwrap();
            let built = rt
                .block_on(router.build(&Task::new("containerd-bench", spec(&image)).unwrap()))
                .unwrap();
            b.iter_custom(|iters| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let expected = output.0.load(Ordering::Relaxed) + 1;
                        let start = Instant::now();
                        tokio::time::timeout(
                            WAIT_BOUND,
                            built.task().spawn(TaskContext::detached()),
                        )
                        .await
                        .expect("real containerd attempt deadline")
                        .expect("real containerd attempt result");
                        total += start.elapsed();
                        assert_eq!(output.0.load(Ordering::Relaxed), expected);
                    }
                    total
                })
            });
            drop(built);
            drop(router);
            rt.block_on(engine.shutdown())
                .expect("containerd finalizer shutdown");
        });
    }
    group.finish();
}

#[cfg(not(feature = "containerd"))]
fn bench_containerd(_: &mut Criterion) {}

criterion_group!(benches, bench_controlled, bench_stop, bench_containerd);

fn main() {
    benchmark_main("container", benches);
}
