//! Public subprocess build, real attempts, and physical stop boundaries.

#[path = "support/process.rs"]
mod process;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use base64::Engine as _;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::{
    fixtures::{RUNTIMES, WAIT_BOUND},
    report::{CaseFamily, benchmark_main, print_suite_header, record_case},
};
use solti_exec::subprocess::{SubprocessBackendConfig, register_subprocess_runner_with_backend};
use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskWorkload};
use solti_runner::RunnerRouter;
use taskvisor::TaskContext;

const BUILD: CaseFamily = CaseFamily::intake(
    "execution/build",
    "SUBPROCESS · BUILD",
    "built task",
    "built tasks",
    "public router build through reusable TaskRef, including script decoding and cwd preparation",
    "manifest, runtime and runner construction; TaskRef drop and runner shutdown; no child is spawned",
);
const ATTEMPT: CaseFamily = CaseFamily::lifecycle(
    "execution/reused/attempt", "SUBPROCESS · REUSED TASK", "completed attempt", "completed attempts",
    "spawn of an already-built TaskRef through output drain, leader reap and released cleanup ownership",
    "runtime, runner, manifest and TaskRef construction; output assertions and terminal runner shutdown",
).without_lifecycle_interpretation();
const FULL: CaseFamily = CaseFamily::lifecycle(
    "execution/build_and_run", "SUBPROCESS · BUILD AND RUN", "completed attempt", "completed attempts",
    "public router build through one real process attempt, output drain, reap and released cleanup ownership",
    "runtime, runner and manifest construction; output assertions and terminal runner shutdown",
).without_lifecycle_interpretation();
const STOP: CaseFamily = CaseFamily::lifecycle(
    "execution/stop", "SUBPROCESS · STOP AND PHYSICAL CLEANUP", "stopped process tree", "stopped process trees",
    "cancel/delete/force-drop request through logical completion, stopped leader and descendant, and released runner ownership",
    "runtime, API, runner, task build, process startup and ready-marker wait; final API and runner shutdown",
).without_lifecycle_interpretation();

fn workload(label: &str) -> TaskWorkload {
    match label {
        "command" => process::command_workload(vec!["quiet".to_owned()]),
        #[cfg(unix)]
        "script_128b" | "script_64kib" => {
            let size = if label == "script_128b" {
                128
            } else {
                64 * 1024
            };
            let command = "printf '%s\\n' solti-bench-done\n";
            let body = format!("#{}\n{command}", "x".repeat(size - command.len() - 2));
            assert_eq!(body.len(), size);
            TaskWorkload::Subprocess(SubprocessSpec::new(
                SubprocessMode::Script {
                    interpreter: "/bin/sh".to_owned(),
                    body: base64::engine::general_purpose::STANDARD.encode(body),
                    args: Vec::new(),
                },
                TaskEnv::new(),
                None,
                Flag::enabled(),
            ))
        }
        _ => unreachable!("unknown benchmark workload"),
    }
}

fn labels() -> &'static [&'static str] {
    #[cfg(unix)]
    {
        &["command", "script_128b", "script_64kib"]
    }
    #[cfg(not(unix))]
    {
        &["command"]
    }
}

fn bench_execution(c: &mut Criterion) {
    print_suite_header("execution");
    for family in [BUILD, ATTEMPT, FULL] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(rt_name, rt_fn) in &RUNTIMES {
            for &label in labels() {
                group.bench_function(BenchmarkId::new(rt_name, label), |b| {
                    record_case(family, rt_name, Some(label.to_owned()));
                    let rt = rt_fn();
                    let output = Arc::new(process::RecordingOutput::default());
                    let mut router = RunnerRouter::new().with_output_publisher(output.clone());
                    let runner = register_subprocess_runner_with_backend(
                        &mut router,
                        "bench-process",
                        SubprocessBackendConfig::new(),
                    )
                    .expect("register subprocess runner");
                    let task = process::task("bench-execution", workload(label));
                    let reused = (family.group_id == ATTEMPT.group_id).then(|| {
                        rt.block_on(router.build(&task))
                            .expect("build reusable task")
                    });
                    b.iter_custom(|iters| {
                        rt.block_on(async {
                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let expected = output.snapshot().done + 1;
                                let start = Instant::now();
                                if family.group_id == BUILD.group_id {
                                    let built =
                                        tokio::time::timeout(WAIT_BOUND, router.build(&task))
                                            .await
                                            .expect("build deadline")
                                            .expect("build subprocess");
                                    total += start.elapsed();
                                    drop(built);
                                } else {
                                    let built;
                                    let runnable = if let Some(reused) = reused.as_ref() {
                                        reused.task()
                                    } else {
                                        built =
                                            router.build(&task).await.expect("build subprocess");
                                        built.task()
                                    };
                                    tokio::time::timeout(
                                        WAIT_BOUND,
                                        runnable.spawn(TaskContext::detached()),
                                    )
                                    .await
                                    .expect("attempt deadline")
                                    .expect("successful child");
                                    process::wait_clean(&runner).await;
                                    total += start.elapsed();
                                    assert_eq!(output.snapshot().done, expected);
                                }
                            }
                            total
                        })
                    });
                    drop(reused);
                    drop(router);
                    rt.block_on(process::shutdown(&runner));
                });
            }
        }
        group.finish();
    }
}

#[cfg(unix)]
fn bench_stop(c: &mut Criterion) {
    use solti_core::SupervisorApi;
    let mut group = c.benchmark_group(STOP.group_id);
    group.throughput(Throughput::Elements(1));
    for &(rt_name, rt_fn) in &RUNTIMES {
        for action in ["cancel", "delete", "force_drop"] {
            group.bench_function(BenchmarkId::new(rt_name, action), |b| {
                record_case(STOP, rt_name, Some(action.to_owned()));
                let rt = rt_fn();
                b.iter_custom(|iters| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iters {
                            let gate = process::Gate::new();
                            let workload = process::command_workload(gate.args("tree"));
                            let mut router = RunnerRouter::new();
                            let runner = register_subprocess_runner_with_backend(
                                &mut router,
                                "bench-stop",
                                SubprocessBackendConfig::new(),
                            )
                            .expect("register stop runner");
                            if action == "force_drop" {
                                let built = router
                                    .build(&process::task("stop-task", workload))
                                    .await
                                    .expect("build process tree");
                                let task = built.into_task();
                                let attempt = tokio::spawn(async move {
                                    task.spawn(TaskContext::detached()).await
                                });
                                let pids = gate.wait_ready().await;
                                let start = Instant::now();
                                attempt.abort();
                                assert!(
                                    attempt
                                        .await
                                        .expect_err("attempt must be aborted")
                                        .is_cancelled()
                                );
                                process::wait_clean(&runner).await;
                                process::wait_not_running(&pids).await;
                                total += start.elapsed();
                                drop(router);
                            } else {
                                let api = SupervisorApi::builder(router)
                                    .start()
                                    .await
                                    .expect("start core API");
                                let task = api
                                    .create_task(process::manifest("stop-task", workload))
                                    .await
                                    .expect("create process tree");
                                let pids = gate.wait_ready().await;
                                let start = Instant::now();
                                if action == "cancel" {
                                    api.cancel_task(task.name()).await.expect("cancel task");
                                    tokio::time::timeout(WAIT_BOUND, async {
                                        loop {
                                            if api
                                                .get_task(task.name())
                                                .expect("retained canceled task")
                                                .phase()
                                                .is_terminal()
                                            {
                                                break;
                                            }
                                            tokio::task::yield_now().await;
                                        }
                                    })
                                    .await
                                    .expect("cancellation terminal state");
                                } else {
                                    api.delete_task(task.name()).await.expect("delete task");
                                    assert!(api.get_task(task.name()).is_none());
                                }
                                process::wait_clean(&runner).await;
                                process::wait_not_running(&pids).await;
                                total += start.elapsed();
                                api.shutdown().await.expect("core API shutdown");
                            }
                            process::shutdown(&runner).await;
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

#[cfg(not(unix))]
fn bench_stop(_: &mut Criterion) {}

criterion_group!(benches, bench_execution, bench_stop);

fn main() {
    if !process::maybe_child() {
        benchmark_main("execution", benches);
    }
}
