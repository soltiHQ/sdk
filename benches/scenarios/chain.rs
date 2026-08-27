//! Conditional chain processes through the public leaf-runner boundary.

#[cfg(feature = "subprocess")]
#[path = "support/process.rs"]
mod process;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::{
    fixtures::{RUNTIMES, WAIT_BOUND},
    report::{CaseFamily, benchmark_main, print_suite_header, record_case},
};
use solti_chain::{ChainSpec, ChainStep, FailureMode, register_chain_runner};
use solti_core::SupervisorApi;
use solti_model::{
    ExtensionWorkload, Task, TaskId, TaskManifest, TaskPhase, TaskSpec, TaskWorkload,
    WorkloadTypeMeta,
};
use solti_runner::{
    BuildCancellation, BuildContext, BuildScope, OutputPublisher, OutputSink, RunId, Runner,
    RunnerError, RunnerRouter,
};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};

const BUILD: CaseFamily = CaseFamily::intake(
    "chain/build_all_branches",
    "CHAIN · COMPILE ALL BRANCHES",
    "compiled chain",
    "compiled chains",
    "router build through validation and every declared leaf build, including the unselected failure handler",
    "runtime, router, graph and deterministic leaf fixture construction; compiled TaskRef drop; no execution",
);
const EXECUTE: CaseFamily = CaseFamily::lifecycle(
    "chain/selected_path", "CHAIN · SELECTED PATH", "chain attempt", "chain attempts",
    "one built chain spawn through selected sequential steps, failure/cancel transition and shared outer output publication",
    "runtime, router and chain build; assertions; leaf workload is controlled in-memory work, not subprocess work",
).without_lifecycle_interpretation();
const RESTART: CaseFamily = CaseFamily::lifecycle(
    "chain/restart_entry",
    "CHAIN · OUTER RETRY",
    "completed chain task",
    "completed chain tasks",
    "core create_task through failure of the last leaf, outer retry from entry with explicit 1ms no-jitter backoff, and terminal success",
    "runtime, API, router and manifest construction; execution-log assertions and API shutdown; controlled in-memory leaves",
).without_lifecycle_interpretation();
#[cfg(feature = "subprocess")]
const PROCESSES: CaseFamily = CaseFamily::lifecycle(
    "chain/real_subprocesses", "CHAIN · REAL SUBPROCESS STEPS", "completed chain attempt", "completed chain attempts",
    "one built chain spawn through real subprocess steps, output drain, child reap and released runner ownership",
    "runtime, runner, chain build; output assertions and terminal runner shutdown",
).without_lifecycle_interpretation();

#[derive(Default)]
struct Observations {
    builds: AtomicUsize,
    executed: Mutex<Vec<usize>>,
    sink_requests: AtomicUsize,
    chunks: Arc<AtomicUsize>,
}

impl OutputPublisher for Observations {
    fn sink_for(&self, _: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        self.sink_requests.fetch_add(1, Ordering::Relaxed);
        let chunks = Arc::clone(&self.chunks);
        Some(OutputSink::new(generation, attempt, move |_| {
            chunks.fetch_add(1, Ordering::Relaxed);
        }))
    }
}

struct ControlledLeaf(Arc<Observations>);

#[solti_runner::async_trait]
impl Runner for ControlledLeaf {
    fn name(&self) -> &str {
        "controlled-leaf"
    }
    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new("benches.solti.io/v1", "Leaf").unwrap()]
    }
    async fn build_task(
        &self,
        task: &Task,
        _: &RunId,
        ctx: &BuildContext,
        _: &BuildCancellation,
        _: &mut BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let TaskWorkload::Extension(workload) = task.spec().workload() else {
            return Err(RunnerError::InvalidSpec(
                "controlled leaf extension required".into(),
            ));
        };
        let index = workload.spec()["index"].as_u64().expect("leaf index") as usize;
        let behavior = workload.spec()["behavior"]
            .as_str()
            .expect("leaf behavior")
            .to_owned();
        self.0.builds.fetch_add(1, Ordering::Relaxed);
        let observations = Arc::clone(&self.0);
        let output = Arc::clone(ctx.output_publisher());
        let name = task.name().clone();
        let generation = task.metadata().generation();
        let attempts = Arc::new(AtomicUsize::new(0));
        Ok(TaskFn::arc(move |_: TaskContext| {
            let observations = Arc::clone(&observations);
            let output = Arc::clone(&output);
            let name = name.clone();
            let behavior = behavior.clone();
            let attempt = attempts.fetch_add(1, Ordering::Relaxed) + 1;
            async move {
                observations.executed.lock().unwrap().push(index);
                if let Some(sink) = output.sink_for(&name, generation, attempt as u32) {
                    sink.stdout_line(Bytes::from_static(b"controlled-leaf-output"));
                }
                match behavior.as_str() {
                    "fail" => Err(TaskError::fail("controlled failure").with_exit_code(17)),
                    "once" if attempt == 1 => {
                        Err(TaskError::fail("controlled first-attempt failure"))
                    }
                    "cancel" => Err(TaskError::Canceled),
                    _ => Ok(()),
                }
            }
        }))
    }
}

fn leaf(index: usize, behavior: &str) -> TaskWorkload {
    TaskWorkload::Extension(
        ExtensionWorkload::new(
            "benches.solti.io/v1",
            "Leaf",
            serde_json::json!({"index": index, "behavior": behavior}),
        )
        .unwrap(),
    )
}

fn graph(steps: usize, mode: &str) -> TaskWorkload {
    let mut nodes = Vec::new();
    for index in 0..steps {
        let behavior = if index + 1 == steps {
            match mode {
                "preserve" | "recover" => "fail",
                "restart" => "once",
                "cancel" => "cancel",
                _ => "ok",
            }
        } else {
            "ok"
        };
        let mut step = ChainStep::new(format!("step-{index}"), leaf(index, behavior)).unwrap();
        if index + 1 < steps {
            step = step.with_on_success(format!("step-{}", index + 1)).unwrap();
        }
        step = step
            .with_on_failure(
                "handler",
                if mode == "recover" {
                    FailureMode::Recover
                } else {
                    FailureMode::Preserve
                },
            )
            .unwrap();
        nodes.push(step);
    }
    nodes.push(ChainStep::new("handler", leaf(steps, "ok")).unwrap());
    ChainSpec::new("step-0", nodes)
        .unwrap()
        .into_workload()
        .unwrap()
}

fn fixture() -> (RunnerRouter, Arc<Observations>) {
    let observations = Arc::new(Observations::default());
    let mut router = RunnerRouter::new().with_output_publisher(observations.clone());
    router
        .register(Arc::new(ControlledLeaf(Arc::clone(&observations))))
        .unwrap();
    register_chain_runner(&mut router, "chain").unwrap();
    (router, observations)
}

fn task(steps: usize, mode: &str) -> Task {
    Task::new(
        "bench-chain",
        TaskSpec::builder("chain-slot", graph(steps, mode), 20_000_u64)
            .build()
            .unwrap(),
    )
    .unwrap()
}

fn bench_chain(c: &mut Criterion) {
    print_suite_header("chain");
    for family in [BUILD, EXECUTE] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(rt_name, rt_fn) in &RUNTIMES {
            for steps in [1, 8, 32] {
                for mode in ["success", "preserve", "recover", "cancel"] {
                    if family.group_id == BUILD.group_id && mode != "success" {
                        continue;
                    }
                    let label = format!("{mode}_{steps}_steps_plus_handler");
                    group.bench_function(BenchmarkId::new(rt_name, &label), |b| {
                        record_case(family, rt_name, Some(label.clone()));
                        let rt = rt_fn();
                        b.iter_custom(|iters| {
                            rt.block_on(async {
                                let mut total = Duration::ZERO;
                                for _ in 0..iters {
                                    let (router, observations) = fixture();
                                    let task = task(steps, mode);
                                    if family.group_id == BUILD.group_id {
                                        let start = Instant::now();
                                        let built = router
                                            .build(&task)
                                            .await
                                            .expect("compile every chain branch");
                                        total += start.elapsed();
                                        assert_eq!(
                                            observations.builds.load(Ordering::Relaxed),
                                            steps + 1
                                        );
                                        assert!(observations.executed.lock().unwrap().is_empty());
                                        drop(built);
                                    } else {
                                        let built = router.build(&task).await.expect("build chain");
                                        let start = Instant::now();
                                        let result = tokio::time::timeout(
                                            WAIT_BOUND,
                                            built.task().spawn(TaskContext::detached()),
                                        )
                                        .await
                                        .expect("chain attempt deadline");
                                        total += start.elapsed();
                                        match mode {
                                            "preserve" => assert!(matches!(
                                                result,
                                                Err(TaskError::Fail {
                                                    exit_code: Some(17),
                                                    ..
                                                })
                                            )),
                                            "cancel" => {
                                                assert!(matches!(result, Err(TaskError::Canceled)))
                                            }
                                            _ => result.expect("successful or recovered chain"),
                                        }
                                        let mut expected = (0..steps).collect::<Vec<_>>();
                                        if matches!(mode, "preserve" | "recover") {
                                            expected.push(steps);
                                        }
                                        assert_eq!(
                                            *observations.executed.lock().unwrap(),
                                            expected
                                        );
                                        assert_eq!(
                                            observations.sink_requests.load(Ordering::Relaxed),
                                            1
                                        );
                                        assert_eq!(
                                            observations.chunks.load(Ordering::Relaxed),
                                            expected.len() * 3
                                        );
                                    }
                                }
                                total
                            })
                        });
                    });
                }
            }
        }
        group.finish();
    }
}

fn bench_restart(c: &mut Criterion) {
    let mut group = c.benchmark_group(RESTART.group_id);
    group.throughput(Throughput::Elements(1));
    for &(rt_name, rt_fn) in &RUNTIMES {
        for steps in [2, 8] {
            let label = format!("{steps}_steps_1ms_backoff");
            group.bench_function(BenchmarkId::new(rt_name, &label), |b| {
                record_case(RESTART, rt_name, Some(label.clone()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let (router, observations) = fixture();
                            let api = SupervisorApi::builder(router)
                                .start()
                                .await
                                .expect("chain API");
                            let spec = TaskSpec::builder(
                                "chain-retry-slot",
                                graph(steps, "restart"),
                                20_000_u64,
                            )
                            .restart(solti_model::RestartPolicy::OnFailure)
                            .backoff(solti_model::BackoffPolicy {
                                jitter: solti_model::JitterPolicy::None,
                                first_ms: 1,
                                max_ms: 1,
                                factor: 1.0,
                            })
                            .build()
                            .unwrap();
                            let manifest = TaskManifest::new("chain-retry", spec).unwrap();
                            let start = Instant::now();
                            let task = api.create_task(manifest).await.expect("create retry chain");
                            tokio::time::timeout(WAIT_BOUND, async {
                                loop {
                                    if *api.get_task(task.name()).unwrap().phase()
                                        == TaskPhase::Succeeded
                                    {
                                        break;
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .expect("chain did not complete its retry");
                            total += start.elapsed();
                            let mut expected = (0..steps).collect::<Vec<_>>();
                            expected.push(steps); // Preserve handler ran before the first outer failure.
                            expected.extend(0..steps);
                            assert_eq!(*observations.executed.lock().unwrap(), expected);
                            api.shutdown().await.expect("chain API shutdown");
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

#[cfg(feature = "subprocess")]
fn bench_processes(c: &mut Criterion) {
    use solti_exec::subprocess::{
        SubprocessBackendConfig, register_subprocess_runner_with_backend,
    };
    let mut group = c.benchmark_group(PROCESSES.group_id);
    group.throughput(Throughput::Elements(1));
    for &(rt_name, rt_fn) in &RUNTIMES {
        for steps in [2, 8] {
            let label = format!("{steps}_command_steps");
            group.bench_function(BenchmarkId::new(rt_name, &label), |b| {
                record_case(PROCESSES, rt_name, Some(label.clone()));
                let rt = rt_fn();
                let output = Arc::new(process::RecordingOutput::default());
                let mut router = RunnerRouter::new().with_output_publisher(output.clone());
                let runner = register_subprocess_runner_with_backend(
                    &mut router,
                    "process-leaf",
                    SubprocessBackendConfig::new(),
                )
                .unwrap();
                register_chain_runner(&mut router, "chain").unwrap();
                let nodes = (0..steps)
                    .map(|index| {
                        let step = ChainStep::new(
                            format!("step-{index}"),
                            process::command_workload(vec!["quiet".into()]),
                        )
                        .unwrap();
                        if index + 1 < steps {
                            step.with_on_success(format!("step-{}", index + 1)).unwrap()
                        } else {
                            step
                        }
                    })
                    .collect();
                let workload = ChainSpec::new("step-0", nodes)
                    .unwrap()
                    .into_workload()
                    .unwrap();
                let built = rt
                    .block_on(router.build(&process::task("process-chain", workload)))
                    .unwrap();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let expected = output.snapshot().done + steps;
                            let start = Instant::now();
                            tokio::time::timeout(
                                WAIT_BOUND,
                                built.task().spawn(TaskContext::detached()),
                            )
                            .await
                            .expect("real chain deadline")
                            .expect("real chain result");
                            process::wait_clean(&runner).await;
                            total += start.elapsed();
                            assert_eq!(output.snapshot().done, expected);
                        }
                        total
                    })
                });
                drop(built);
                drop(router);
                rt.block_on(process::shutdown(&runner));
            });
        }
    }
    group.finish();
}

#[cfg(not(feature = "subprocess"))]
fn bench_processes(_: &mut Criterion) {}

criterion_group!(benches, bench_chain, bench_restart, bench_processes);

fn main() {
    #[cfg(feature = "subprocess")]
    if process::maybe_child() {
        return;
    }
    benchmark_main("chain", benches);
}
