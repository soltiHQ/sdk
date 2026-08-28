//! Project processes: SDK resource lifecycles, retries, and shared-slot policies.
//! Controlled in-process tasks keep external process startup outside this suite.

mod core_support;

use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::fixtures::{RUNTIMES, bounded, embedded_manifest, wait_task, wait_terminal};
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};
use solti_core::SupervisorApi;
use solti_model::{
    AdmissionPolicy, BackoffPolicy, JitterPolicy, RestartPolicy, TaskManifest, TaskPhase,
    TaskRunQuery, TaskSpec,
};
use solti_runner::RunnerRouter;
use taskvisor::{TaskContext, TaskError, TaskFn};

use core_support::{
    BusyRejections, ControlledRunner, Counter, held_task, immediate_task, marked_task,
    retained_task, routed_manifest, router,
};

const COLD: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/cold/full_run",
    "COLD SDK RESOURCE · FULL RUN",
    "completed resource lifecycle",
    "completed resource lifecycles",
    "fresh SupervisorApi startup, Queue desired commit, Succeeded observation, and shared SDK shutdown",
    "Tokio runtime, runner registration, manifest and embedded TaskRef construction",
)
.without_lifecycle_interpretation();

const CYCLES: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/steady/resource_cycles",
    "WARM SDK · RESOURCE CYCLES",
    "completed resource cycle",
    "completed resource cycles",
    "batch of Queue desired creates through Succeeded observations and public deletion of every resource",
    "Tokio and supervisor startup, warmup, manifests, TaskRefs, final shutdown",
)
.without_lifecycle_interpretation();

const RETRIES: CaseFamily = CaseFamily::lifecycle(
    "lifecycle/retry/history_cycle",
    "RETRY CYCLE · RETAINED ATTEMPTS",
    "completed retry cycle", "completed retry cycles",
    "Queue desired create, controlled retry failures with fixed 1 ms policy backoff, final success, public settlement, and run-history read",
    "Tokio and supervisor startup, fixture construction, resource deletion and shutdown",
).without_lifecycle_interpretation();

const BUSY: CaseFamily = CaseFamily::policy(
    "lifecycle/slot/drop_busy",
    "SHARED SLOT · VERIFIED BUSY REJECTIONS",
    "verified busy rejection",
    "verified busy rejections",
    "desired submissions through typed SlotBusy events and terminal SDK projections, with one counting subscriber",
    "supervisor startup, active owner readiness, request construction, owner release, deletion and shutdown",
);

const QUEUE: CaseFamily = CaseFamily::policy(
    "lifecycle/slot/queue",
    "SHARED SLOT · QUEUED SUCCESSES",
    "observed task success",
    "observed task successes",
    "public Queue submissions through Succeeded SDK projections in one slot",
    "supervisor startup, request construction, deletion and shutdown; terminal projection alone is not physical cleanup",
);

const REPLACE: CaseFamily = CaseFamily::policy(
    "lifecycle/slot/replace",
    "SHARED SLOT · COOPERATIVE REPLACEMENT",
    "verified replacement",
    "verified replacements",
    "replacement desired commit through the old body's cooperative cancellation and successor Succeeded projection",
    "supervisor startup, active owner readiness, request construction, deletion and shutdown",
);

fn with_policy(name: &str, slot: &str, policy: AdmissionPolicy) -> TaskManifest {
    let base = embedded_manifest(name, slot);
    TaskManifest::new(name, base.spec().clone().with_admission(policy))
        .expect("valid slot manifest")
}

fn bench_cold(c: &mut Criterion) {
    print_suite_header("lifecycle");
    let mut group = c.benchmark_group(COLD.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for routed in [false, true] {
            let path = if routed {
                "controlled_routed"
            } else {
                "embedded"
            };
            group.bench_with_input(
                BenchmarkId::new(runtime_name, path),
                &routed,
                |b, &routed| {
                    record_case(COLD, runtime_name, Some(path.to_owned()));
                    let rt = runtime_factory();
                    b.iter_custom(|iterations| {
                        rt.block_on(async {
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let runner = Arc::new(ControlledRunner::new("cold-runner"));
                                let router = if routed {
                                    router(runner)
                                } else {
                                    RunnerRouter::new()
                                };
                                let manifest = if routed {
                                    routed_manifest("cold-task", "cold-slot", 1, None)
                                } else {
                                    embedded_manifest("cold-task", "cold-slot")
                                };
                                let task_ref = immediate_task();
                                let started = Instant::now();
                                let api = bounded(SupervisorApi::builder(router).start())
                                    .await
                                    .expect("cold SDK startup");
                                let task = if routed {
                                    bounded(api.create_task(manifest)).await
                                } else {
                                    bounded(api.create_embedded_task(manifest, task_ref)).await
                                }
                                .expect("cold desired commit");
                                let terminal = wait_terminal(&api, task.name()).await;
                                assert_eq!(terminal.phase(), &TaskPhase::Succeeded);
                                bounded(api.shutdown()).await.expect("cold SDK shutdown");
                                total += started.elapsed();
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

fn bench_resource_cycles(c: &mut Criterion) {
    let mut group = c.benchmark_group(CYCLES.group_id);
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for routed in [false, true] {
            for count in [1_usize, 32] {
                let variant = format!(
                    "{}_{}",
                    if routed {
                        "controlled_routed"
                    } else {
                        "embedded"
                    },
                    count
                );
                group.throughput(Throughput::Elements(count as u64));
                group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                    record_case(CYCLES, runtime_name, Some(variant.clone()));
                    let rt = runtime_factory();
                    b.iter_custom(|iterations| {
                        rt.block_on(async {
                            let api = bounded(
                                SupervisorApi::builder(router(Arc::new(ControlledRunner::new(
                                    "cycle-runner",
                                ))))
                                .start(),
                            )
                            .await
                            .expect("SDK startup");
                            let warm = retained_task(&api, embedded_manifest("warm", "warm")).await;
                            bounded(api.delete_task(warm.name()))
                                .await
                                .expect("warmup cleanup");
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let requests = (0..count)
                                    .map(|index| {
                                        let name = format!("cycle-{index}");
                                        let manifest = if routed {
                                            routed_manifest(&name, &name, 1, None)
                                        } else {
                                            embedded_manifest(&name, &name)
                                        };
                                        (manifest, immediate_task())
                                    })
                                    .collect::<Vec<_>>();
                                let mut names = Vec::with_capacity(count);
                                let started = Instant::now();
                                for (manifest, task_ref) in requests {
                                    let task = if routed {
                                        bounded(api.create_task(manifest)).await
                                    } else {
                                        bounded(api.create_embedded_task(manifest, task_ref)).await
                                    }
                                    .expect("desired commit");
                                    names.push(task.name().clone());
                                }
                                for name in &names {
                                    assert_eq!(
                                        wait_terminal(&api, name).await.phase(),
                                        &TaskPhase::Succeeded
                                    );
                                    bounded(api.delete_task(name))
                                        .await
                                        .expect("resource cleanup");
                                }
                                total += started.elapsed();
                                assert!(names.iter().all(|name| api.get_task(name).is_none()));
                            }
                            bounded(api.shutdown()).await.expect("SDK shutdown");
                            total
                        })
                    });
                });
            }
        }
    }
    group.finish();
}

fn bench_retries(c: &mut Criterion) {
    let mut group = c.benchmark_group(RETRIES.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for failures in [1_u32, 4] {
            let variant = format!("{failures}_failures_then_success");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(RETRIES, runtime_name, Some(variant.clone()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let base = embedded_manifest("retry", "retry");
                            let spec = TaskSpec::builder(
                                "retry",
                                base.spec().workload().clone(),
                                30_000_u64,
                            )
                            .admission(AdmissionPolicy::Queue)
                            .restart(RestartPolicy::OnFailure)
                            .max_retries(NonZeroU32::new(failures + 1))
                            .backoff(BackoffPolicy {
                                jitter: JitterPolicy::None,
                                first_ms: 1,
                                max_ms: 1,
                                factor: 1.0,
                            })
                            .build()
                            .expect("valid bounded retry spec");
                            let manifest =
                                TaskManifest::new("retry", spec).expect("valid retry manifest");
                            let attempts = Arc::new(AtomicUsize::new(0));
                            let execution_attempts = Arc::clone(&attempts);
                            let task_ref = TaskFn::arc(move |_ctx: TaskContext| {
                                let attempts = Arc::clone(&execution_attempts);
                                async move {
                                    if attempts.fetch_add(1, Ordering::AcqRel) < failures as usize {
                                        Err(TaskError::fail("controlled retryable failure"))
                                    } else {
                                        Ok(())
                                    }
                                }
                            });
                            let started = Instant::now();
                            let task = bounded(api.create_embedded_task(manifest, task_ref))
                                .await
                                .expect("retry create");
                            // An intermediate Failed phase is not a completed retry cycle.
                            wait_task(&api, task.name(), |task| {
                                task.phase() == &TaskPhase::Succeeded
                            })
                            .await;
                            bounded(api.cancel_task(task.name()))
                                .await
                                .expect("retry finalization");
                            let history = api
                                .query_task_runs(task.name(), &TaskRunQuery::new())
                                .expect("run query")
                                .expect("retained retry resource");
                            total += started.elapsed();
                            assert_eq!(attempts.load(Ordering::Acquire), failures as usize + 1);
                            assert_eq!(
                                history.items.len(),
                                failures as usize + 1,
                                "this small history fixture must retain every observed attempt"
                            );
                            assert!(history.items.iter().all(|run| !run.is_active()));
                            bounded(api.delete_task(task.name()))
                                .await
                                .expect("retry cleanup");
                        }
                        bounded(api.shutdown()).await.expect("SDK shutdown");
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

fn bench_slot_policies(c: &mut Criterion) {
    for family in [BUSY, QUEUE, REPLACE] {
        let mut group = c.benchmark_group(family.group_id);
        let count = if family.group_id == REPLACE.group_id {
            1
        } else {
            16
        };
        group.throughput(Throughput::Elements(count as u64));
        for &(runtime_name, runtime_factory) in &RUNTIMES {
            group.bench_function(runtime_name, |b| {
                record_case(family, runtime_name, None);
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let observer = Arc::new(BusyRejections::default());
                        let api = bounded(
                            SupervisorApi::builder(RunnerRouter::new())
                                .with_subscribers(vec![observer.clone()])
                                .start(),
                        )
                        .await
                        .expect("SDK startup");
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let owner_started = Counter::new();
                            let owner_release = Counter::new();
                            let owner_canceled = Counter::new();
                            let queue = family.group_id == QUEUE.group_id;
                            let replace = family.group_id == REPLACE.group_id;
                            let owner = if queue {
                                None
                            } else {
                                let owner = bounded(api.create_embedded_task(
                                    embedded_manifest("owner", "shared-slot"),
                                    held_task(
                                        owner_started.clone(),
                                        owner_release.clone(),
                                        owner_canceled.clone(),
                                    ),
                                ))
                                .await
                                .expect("owner create");
                                owner_started.wait(1).await;
                                Some(owner)
                            };
                            let policy = if queue {
                                AdmissionPolicy::Queue
                            } else if replace {
                                AdmissionPolicy::Replace
                            } else {
                                AdmissionPolicy::DropIfRunning
                            };
                            let candidate_starts = Counter::new();
                            let requests = (0..count)
                                .map(|index| {
                                    (
                                        with_policy(
                                            &format!("candidate-{index}"),
                                            "shared-slot",
                                            policy,
                                        ),
                                        marked_task(candidate_starts.clone()),
                                    )
                                })
                                .collect::<Vec<_>>();
                            let before_rejections = observer.count.get();
                            let mut names = Vec::with_capacity(count);
                            let started = Instant::now();
                            for (manifest, task_ref) in requests {
                                names.push(
                                    bounded(api.create_embedded_task(manifest, task_ref))
                                        .await
                                        .expect("slot create")
                                        .name()
                                        .clone(),
                                );
                            }
                            if !queue && !replace {
                                observer.count.wait(before_rejections + count).await;
                            }
                            if replace {
                                owner_canceled.wait(1).await;
                            }
                            for name in &names {
                                let expected = if queue || replace {
                                    TaskPhase::Succeeded
                                } else {
                                    TaskPhase::Canceled
                                };
                                assert_eq!(wait_terminal(&api, name).await.phase(), &expected);
                            }
                            total += started.elapsed();
                            assert_eq!(
                                candidate_starts.get(),
                                if queue || replace { count } else { 0 }
                            );
                            if let Some(owner) = owner {
                                owner_release.increment();
                                bounded(api.delete_task(owner.name()))
                                    .await
                                    .expect("owner cleanup");
                            }
                            for name in &names {
                                bounded(api.delete_task(name))
                                    .await
                                    .expect("candidate cleanup");
                            }
                        }
                        bounded(api.shutdown()).await.expect("SDK shutdown");
                        total
                    })
                });
            });
        }
        group.finish();
    }
}

criterion_group!(
    benches,
    bench_cold,
    bench_resource_cycles,
    bench_retries,
    bench_slot_policies
);

fn main() {
    benchmark_main("lifecycle", benches);
}
