//! Desired-state writes, latest-wins convergence, and bounded runner construction.
//! Retained-state admission rejects; runner-build admission waits. They are
//! deliberately different result families.

mod core_support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::fixtures::{RUNTIMES, bounded, embedded_manifest, wait_task, wait_terminal};
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};
use solti_core::{CoreError, ReconciliationConfig, StateConfig, SupervisorApi};
use solti_model::{Annotations, ConditionStatus, TaskManifest, TaskPhase, WritePreconditions};
use solti_runner::RunnerRouter;
use tokio::sync::Semaphore;

use core_support::{
    ControlledRunner, Counter, embedded_revision, held_task, immediate_task, labeled_router,
    observed, retained_task, routed_manifest, router, with_label,
};

const NOOP: CaseFamily = CaseFamily::intake(
    "reconciliation/apply/noop",
    "DESIRED APPLY · NO CHANGE",
    "verified no-op apply",
    "verified no-op applies",
    "public identical apply through its unchanged desired-state acknowledgement",
    "stable retained fixture, manifests, TaskRefs, supervisor startup and shutdown; no runtime is rebuilt",
);
const METADATA: CaseFamily = CaseFamily::intake(
    "reconciliation/apply/metadata",
    "DESIRED APPLY · METADATA ONLY",
    "metadata commit",
    "metadata commits",
    "public label-only apply through its new resource-version acknowledgement",
    "stable retained fixture, alternating manifests, TaskRefs, startup and shutdown; generation does not change",
);
const GUARDED: CaseFamily = CaseFamily::intake(
    "reconciliation/apply/matching_preconditions",
    "DESIRED APPLY · MATCHING GUARDS",
    "guarded no-op apply",
    "guarded no-op applies",
    "identical apply with matching UID and resource-version preconditions through acknowledgement",
    "stable fixture, guards and request construction, startup and shutdown",
);
const CONFLICT: CaseFamily = CaseFamily::policy(
    "reconciliation/apply/stale_precondition",
    "DESIRED APPLY · STALE GUARD",
    "verified write conflict",
    "verified write conflicts",
    "public apply through verified Conflict for an outdated resource-version guard",
    "stable fixture, guards and request construction, startup and shutdown",
);
const SPEC: CaseFamily = CaseFamily::policy(
    "reconciliation/apply/spec_replacement",
    "SPEC APPLY · REPLACEMENT CONVERGENCE",
    "converged generation",
    "converged generations",
    "Queue spec apply through new task-body entry and the matching observed generation",
    "initial running resource, next manifest and TaskRef construction, deletion and shutdown; body entry is not task completion",
);
const RETRY: CaseFamily = CaseFamily::policy(
    "reconciliation/apply/failed_build_retry",
    "IDENTICAL APPLY · FAILED BUILD RETRY",
    "successful reconciliation retry",
    "successful reconciliation retries",
    "identical apply after a controlled build failure through Succeeded SDK projection",
    "supervisor startup, first failed build, request construction, settlement, deletion and shutdown",
);
const RETAINED_COUNT: CaseFamily = CaseFamily::policy(
    "reconciliation/retained/count_rejection",
    "RETAINED STATE · COUNT REJECTION",
    "verified retained-count rejection",
    "verified retained-count rejections",
    "new-name create through RetainedTaskLimitReached at the configured count limit",
    "retained fixture, request construction, startup and shutdown; this admission rejects without waiting",
);
const RETAINED_BYTES: CaseFamily = CaseFamily::policy(
    "reconciliation/retained/manifest_growth_rejection",
    "RETAINED STATE · MANIFEST BYTE REJECTION",
    "verified manifest-growth rejection",
    "verified manifest-growth rejections",
    "positive-growth existing apply through RetainedTaskManifestByteLimitExceeded at an exact compact-JSON budget",
    "fixture serialization and population, request construction, startup and shutdown; rejected applies do not mutate state",
);
const LATEST: CaseFamily = CaseFamily::policy(
    "reconciliation/latest_wins/burst",
    "LATEST WINS · UPDATE BURST",
    "converged update burst",
    "converged update bursts",
    "successive spec commits over a blocked build through the final generation's Succeeded projection",
    "initial blocked generation readiness, manifest construction, settlement, deletion and shutdown; intermediate builds may be coalesced",
);
const BUILDS: CaseFamily = CaseFamily::policy(
    "reconciliation/build_admission/controlled_burst",
    "MANAGED BUILD ADMISSION · CONTROLLED BURST",
    "observed routed success",
    "observed routed successes",
    "routed creates, confirmed build-gate saturation, gate release, and all Succeeded SDK projections",
    "supervisor startup, request construction, final settlement and shutdown; controlled gate coordination is included",
);

fn bench_apply(c: &mut Criterion) {
    print_suite_header("reconciliation");
    for family in [NOOP, METADATA, GUARDED, CONFLICT] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(runtime_name, runtime_factory) in &RUNTIMES {
            group.bench_function(runtime_name, |b| {
                record_case(family, runtime_name, None);
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        let read = retained_task(&api, embedded_manifest("apply", "apply")).await;
                        let guards =
                            WritePreconditions::from_task(&read).expect("valid read guards");
                        let stable = if family.group_id == CONFLICT.group_id {
                            // Another committed write makes an actual previously
                            // observed resource version stale before measurement.
                            bounded(api.apply_embedded_task(
                                with_label(TaskManifest::from(read), "after-read"),
                                immediate_task(),
                            ))
                            .await
                            .expect("fixture write after the guarded read")
                        } else {
                            read
                        };
                        let original = TaskManifest::from(stable.clone());
                        let versions = [
                            with_label(original.clone(), "a"),
                            with_label(original.clone(), "b"),
                        ];
                        let source = immediate_task();
                        let mut previous_version = stable.metadata().resource_version().to_owned();
                        let mut total = Duration::ZERO;
                        for index in 0..iterations {
                            let manifest = if family.group_id == METADATA.group_id {
                                versions[index as usize % 2].clone()
                            } else {
                                original.clone()
                            };
                            let source = source.clone();
                            let preconditions = guards.clone();
                            let started = Instant::now();
                            let result = if family.group_id == GUARDED.group_id
                                || family.group_id == CONFLICT.group_id
                            {
                                bounded(api.apply_embedded_task_with_preconditions(
                                    manifest,
                                    source,
                                    preconditions,
                                ))
                                .await
                            } else {
                                bounded(api.apply_embedded_task(manifest, source)).await
                            };
                            total += started.elapsed();
                            if family.group_id == CONFLICT.group_id {
                                assert!(matches!(result, Err(CoreError::Conflict(_))));
                            } else {
                                let committed = result.expect("apply acknowledgement");
                                assert_eq!(
                                    committed.metadata().generation(),
                                    stable.metadata().generation()
                                );
                                if family.group_id == METADATA.group_id {
                                    assert_ne!(
                                        committed.metadata().resource_version(),
                                        previous_version
                                    );
                                    previous_version =
                                        committed.metadata().resource_version().to_owned();
                                } else {
                                    assert_eq!(committed, stable);
                                }
                            }
                        }
                        if family.group_id == CONFLICT.group_id {
                            assert_eq!(api.get_task(stable.name()).as_ref(), Some(&stable));
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

fn bench_spec_replacement(c: &mut Criterion) {
    let mut group = c.benchmark_group(SPEC.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        group.bench_function(runtime_name, |b| {
            record_case(SPEC, runtime_name, None);
            let rt = runtime_factory();
            b.iter_custom(|iterations| {
                rt.block_on(async {
                    let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                        .await
                        .expect("SDK startup");
                    let initial_started = Counter::new();
                    let created = bounded(api.create_embedded_task(
                        embedded_revision("replace", "replace", 1),
                        held_task(initial_started.clone(), Counter::new(), Counter::new()),
                    ))
                    .await
                    .expect("initial create");
                    initial_started.wait(1).await;
                    observed(&api, created.name(), 1).await;
                    let mut total = Duration::ZERO;
                    for index in 0..iterations {
                        let generation = index + 2;
                        let manifest = embedded_revision("replace", "replace", generation);
                        let next_started = Counter::new();
                        let task_ref =
                            held_task(next_started.clone(), Counter::new(), Counter::new());
                        let started = Instant::now();
                        let applied = bounded(api.apply_embedded_task(manifest, task_ref))
                            .await
                            .expect("spec apply");
                        next_started.wait(1).await;
                        let accepted = observed(&api, applied.name(), generation).await;
                        total += started.elapsed();
                        assert_eq!(accepted.metadata().generation(), generation);
                    }
                    bounded(api.delete_task(created.name()))
                        .await
                        .expect("replacement cleanup");
                    bounded(api.shutdown()).await.expect("SDK shutdown");
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_failed_retry(c: &mut Criterion) {
    let mut group = c.benchmark_group(RETRY.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        group.bench_function(runtime_name, |b| {
            record_case(RETRY, runtime_name, None);
            let rt = runtime_factory();
            b.iter_custom(|iterations| {
                rt.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iterations {
                        let mut runner = ControlledRunner::new("retry-build");
                        runner.fail_first = true;
                        let runner = Arc::new(runner);
                        let api = bounded(SupervisorApi::builder(router(runner.clone())).start())
                            .await
                            .expect("SDK startup");
                        let manifest = routed_manifest("retry-build", "retry-build", 1, None);
                        let first = bounded(api.create_task(manifest.clone()))
                            .await
                            .expect("first commit");
                        let failed = wait_task(&api, first.name(), |task| {
                            task.status().reconciled().status() == ConditionStatus::False
                        })
                        .await;
                        assert_eq!(failed.status().reconciled().reason(), "RunnerBuildFailed");
                        let started = Instant::now();
                        let retry = bounded(api.apply_task(manifest))
                            .await
                            .expect("retry commit");
                        assert_eq!(
                            wait_terminal(&api, retry.name()).await.phase(),
                            &TaskPhase::Succeeded
                        );
                        total += started.elapsed();
                        assert_eq!(retry.metadata().generation(), 1);
                        assert_eq!(runner.builds.get(), 2);
                        assert_eq!(runner.task_starts.get(), 1);
                        bounded(api.delete_task(first.name()))
                            .await
                            .expect("retry cleanup");
                        bounded(api.shutdown()).await.expect("SDK shutdown");
                    }
                    total
                })
            });
        });
    }
    group.finish();
}

fn bench_retained_admission(c: &mut Criterion) {
    for family in [RETAINED_COUNT, RETAINED_BYTES] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(runtime_name, runtime_factory) in &RUNTIMES {
            group.bench_function(runtime_name, |b| {
                record_case(family, runtime_name, None);
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let base = embedded_manifest("retained", "retained");
                        let byte_limit = serde_json::to_vec(&base)
                            .expect("fixture serialization")
                            .len();
                        let config = if family.group_id == RETAINED_COUNT.group_id {
                            StateConfig::new()
                                .try_with_max_retained_tasks(1)
                                .expect("count limit")
                        } else {
                            StateConfig::new()
                                .try_with_max_retained_task_manifest_bytes(byte_limit)
                                .expect("byte limit")
                        };
                        let api = bounded(
                            SupervisorApi::builder(RunnerRouter::new())
                                .with_state_config(config)
                                .start(),
                        )
                        .await
                        .expect("SDK startup");
                        let stable = retained_task(&api, base.clone()).await;
                        let mut annotations = Annotations::new();
                        annotations.insert("bench.example.org/payload", "positive-manifest-growth");
                        let denied = if family.group_id == RETAINED_COUNT.group_id {
                            embedded_manifest("new-name", "new-name")
                        } else {
                            base.clone()
                                .with_annotations(annotations)
                                .expect("valid growth manifest")
                        };
                        let source = immediate_task();
                        // Saturation must still permit the existing resource's no-op.
                        assert_eq!(
                            bounded(api.apply_embedded_task(base, source.clone()))
                                .await
                                .expect("saturated no-op"),
                            stable
                        );
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let manifest = denied.clone();
                            let task_ref = source.clone();
                            let started = Instant::now();
                            let result = if family.group_id == RETAINED_COUNT.group_id {
                                bounded(api.create_embedded_task(manifest, task_ref)).await
                            } else {
                                bounded(api.apply_embedded_task(manifest, task_ref)).await
                            };
                            total += started.elapsed();
                            if family.group_id == RETAINED_COUNT.group_id {
                                assert!(matches!(
                                    result,
                                    Err(CoreError::RetainedTaskLimitReached { .. })
                                ));
                            } else {
                                assert!(matches!(
                                    result,
                                    Err(CoreError::RetainedTaskManifestByteLimitExceeded { .. })
                                ));
                            }
                        }
                        assert_eq!(api.get_task(stable.name()).as_ref(), Some(&stable));
                        bounded(api.shutdown()).await.expect("SDK shutdown");
                        total
                    })
                });
            });
        }
        group.finish();
    }
}

fn bench_latest_wins(c: &mut Criterion) {
    let mut group = c.benchmark_group(LATEST.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for updates in [8_u64, 32] {
            let variant = format!("{updates}_updates");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(LATEST, runtime_name, Some(variant.clone()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let final_generation = updates + 1;
                            let mut runner = ControlledRunner::new("latest");
                            runner.block_before_generation = Some(final_generation);
                            let runner = Arc::new(runner);
                            let api =
                                bounded(SupervisorApi::builder(router(runner.clone())).start())
                                    .await
                                    .expect("SDK startup");
                            let initial = bounded(
                                api.create_task(routed_manifest("latest", "latest", 1, None)),
                            )
                            .await
                            .expect("initial create");
                            runner.builds.wait(1).await;
                            let requests = (2..=final_generation)
                                .map(|generation| {
                                    routed_manifest("latest", "latest", generation, None)
                                })
                                .collect::<Vec<_>>();
                            let started = Instant::now();
                            for request in requests {
                                bounded(api.apply_task(request))
                                    .await
                                    .expect("latest-wins apply");
                            }
                            let finished = wait_task(&api, initial.name(), |task| {
                                task.metadata().generation() == final_generation
                                    && task.phase() == &TaskPhase::Succeeded
                            })
                            .await;
                            total += started.elapsed();
                            assert_eq!(finished.metadata().generation(), final_generation);
                            assert_eq!(
                                runner.task_starts.get(),
                                1,
                                "obsolete controlled builds must never produce a runtime"
                            );
                            assert!(runner.builds.get() <= final_generation as usize);
                            bounded(api.delete_task(initial.name()))
                                .await
                                .expect("latest-wins cleanup");
                            bounded(api.shutdown()).await.expect("SDK shutdown");
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

fn bench_build_admission(c: &mut Criterion) {
    let mut group = c.benchmark_group(BUILDS.group_id);
    const COUNT: usize = 16;
    group.throughput(Throughput::Elements(COUNT as u64));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for (variant, global, per_runner, mixed) in [
            ("global_2_runner_4", 2, 4, false),
            ("global_4_runner_2", 4, 2, false),
            ("global_4_two_runners_2_each", 4, 2, true),
        ] {
            group.bench_function(BenchmarkId::new(runtime_name, variant), |b| {
                record_case(BUILDS, runtime_name, Some(variant.to_owned()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let gate_a = Arc::new(Semaphore::new(0));
                            let gate_b = Arc::new(Semaphore::new(0));
                            let mut a = ControlledRunner::new("a");
                            a.permits = Some(gate_a.clone());
                            let mut b = ControlledRunner::new("b");
                            b.permits = Some(gate_b.clone());
                            let (a, b) = (Arc::new(a), Arc::new(b));
                            let config = ReconciliationConfig::new()
                                .try_with_max_concurrent_builds(global)
                                .expect("global limit")
                                .try_with_max_concurrent_builds_per_runner(per_runner)
                                .expect("runner limit");
                            let api = bounded(
                                SupervisorApi::builder(labeled_router(&[a.clone(), b.clone()]))
                                    .with_reconciliation_config(config)
                                    .start(),
                            )
                            .await
                            .expect("SDK startup");
                            let requests = (0..COUNT)
                                .map(|index| {
                                    let name = format!("build-{index}");
                                    routed_manifest(
                                        &name,
                                        &name,
                                        1,
                                        Some(if mixed && index >= COUNT / 2 {
                                            "b"
                                        } else {
                                            "a"
                                        }),
                                    )
                                })
                                .collect::<Vec<_>>();
                            let mut names = Vec::with_capacity(COUNT);
                            let started = Instant::now();
                            for request in requests {
                                names.push(
                                    bounded(api.create_task(request))
                                        .await
                                        .expect("build create")
                                        .name()
                                        .clone(),
                                );
                            }
                            let saturation = global.min(per_runner);
                            a.builds.wait(saturation).await;
                            if mixed {
                                b.builds.wait(per_runner).await;
                            }
                            assert_eq!(a.active.load(Ordering::Acquire), saturation);
                            if mixed {
                                assert_eq!(b.active.load(Ordering::Acquire), per_runner);
                            }
                            gate_a.add_permits(COUNT);
                            gate_b.add_permits(COUNT);
                            for name in &names {
                                assert_eq!(
                                    wait_terminal(&api, name).await.phase(),
                                    &TaskPhase::Succeeded
                                );
                            }
                            total += started.elapsed();
                            assert_eq!(a.builds.get() + b.builds.get(), COUNT);
                            assert_eq!(a.task_starts.get() + b.task_starts.get(), COUNT);
                            assert!(a.peak.load(Ordering::Acquire) <= global.min(per_runner));
                            assert!(b.peak.load(Ordering::Acquire) <= global.min(per_runner));
                            bounded(api.shutdown()).await.expect("SDK shutdown");
                        }
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_apply,
    bench_spec_replacement,
    bench_failed_retry,
    bench_retained_admission,
    bench_latest_wins,
    bench_build_admission
);

fn main() {
    benchmark_main("reconciliation", benches);
}
