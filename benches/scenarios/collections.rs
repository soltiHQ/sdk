//! Snapshot pagination, watch delivery, and retained-run collection processes.

mod core_support;

use std::hint::black_box;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use solti_benches::fixtures::{RUNTIMES, bounded, embedded_manifest, wait_task};
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};
use solti_core::{CollectionError, StateConfig, SupervisorApi, TaskWatchSubscription};
use solti_model::{
    Annotations, Labels, Slot, Task, TaskFilter, TaskId, TaskManifest, TaskPhase, TaskQuery,
    TaskRun, TaskRunQuery, TaskWatchEvent,
};
use solti_runner::RunnerRouter;
use tokio_stream::StreamExt;

use core_support::{embedded_revision, immediate_task, retained_task, with_label};

const LIST: CaseFamily = CaseFamily::query(
    "collections/list/full_snapshot",
    "TASK LIST · FULL PAGINATED SNAPSHOT",
    "snapshot traversal",
    "snapshot traversals",
    "all first and continuation page calls through one verified snapshot traversal",
    "retained resource population, query construction, startup and shutdown; each unit is a traversal, not one returned Task",
);
const INTERLEAVED: CaseFamily = CaseFamily::query(
    "collections/list/interleaved_mutation",
    "TASK LIST · SNAPSHOT ACROSS LIVE WRITES",
    "snapshot read/write cycle",
    "snapshot read/write cycles",
    "first page, eight public metadata commits, then remaining pages from the original snapshot",
    "population, expected snapshot and manifest construction, startup and shutdown",
);
const INITIAL: CaseFamily = CaseFamily::query(
    "collections/watch/initial_snapshot",
    "WATCH · INITIAL SNAPSHOT",
    "delivered snapshot item",
    "delivered snapshot items",
    "watch construction from version 0 through every sorted initial Added event",
    "retained resource population, watch drop, startup and shutdown",
);
const REPLAY: CaseFamily = CaseFamily::query(
    "collections/watch/replay",
    "WATCH · RETAINED CHANGE REPLAY",
    "replayed change",
    "replayed changes",
    "exact-version watch construction through eight retained Modified events",
    "baseline capture and metadata commits, stable fixture, watch drop, startup and shutdown",
);
const LIVE: CaseFamily = CaseFamily::query(
    "collections/watch/live_fanout",
    "WATCH · LIVE CHANGE FAN-OUT",
    "delivered change",
    "delivered changes",
    "eight metadata commits through delivery of every change to every existing watcher",
    "watch registration, request construction, population, startup and shutdown; throughput counts deliveries, including fan-out",
);
const EXPIRED: CaseFamily = CaseFamily::policy(
    "collections/watch/expired_position",
    "WATCH · COMPACTED POSITION",
    "verified expired position",
    "verified expired positions",
    "old-version resume rejection or one slow live watch's terminal ResourceVersionExpired result",
    "journal overflow and subscription setup, population, startup and shutdown",
);
const WATCH_ADMISSION: CaseFamily = CaseFamily::policy(
    "collections/watch/count_rejection",
    "WATCH · CONCURRENT ADMISSION LIMIT",
    "verified watch rejection",
    "verified watch rejections",
    "watch construction through ConcurrentTaskWatchLimitReached while one lease occupies the configured limit",
    "retained fixture, occupying watch, startup and shutdown",
);
const RUNS: CaseFamily = CaseFamily::query(
    "collections/runs/full_snapshot",
    "RUN HISTORY · FULL PAGINATED SNAPSHOT",
    "run-history traversal",
    "run-history traversals",
    "all run-history page calls through one verified ordered generation snapshot",
    "public Queue execution of fixture generations, query construction, startup and shutdown",
);
const RUN_CAP: CaseFamily = CaseFamily::query(
    "collections/runs/snapshot_across_eviction",
    "RUN HISTORY · CONTINUATION ACROSS CAP EVICTION",
    "history retention cycle",
    "history retention cycles",
    "first run page, one public Queue generation execution and settlement that evicts the oldest completed run, then original continuation pages",
    "initial four retained generations, expected snapshot and manifest construction, startup and shutdown",
);
#[cfg(feature = "fixtures")]
const SWEEP: CaseFamily = CaseFamily::query(
    "collections/retention/terminal_sweep",
    "RETENTION · TERMINAL RESOURCE SWEEP",
    "removed terminal resource",
    "removed terminal resources",
    "explicit synchronous retention sweep of completed, unbound resources with zero run/task TTL",
    "public lifecycle fixture population, zero-TTL config, startup and shutdown; uses the test-util sweep entrypoint, not worker scheduling",
);

async fn populate(api: &SupervisorApi, count: usize, payload: usize) -> Vec<Task> {
    let mut tasks = Vec::with_capacity(count);
    for index in 0..count {
        let name = format!("item-{index:04}");
        let mut labels = Labels::new();
        labels.insert("cohort", if index % 2 == 0 { "blue" } else { "green" });
        let mut annotations = Annotations::new();
        if payload > 0 {
            annotations.insert("bench.example.org/payload", "x".repeat(payload));
        }
        let manifest = embedded_manifest(&name, &format!("slot-{}", index % 4))
            .with_labels(labels)
            .expect("valid labels")
            .with_annotations(annotations)
            .expect("valid payload");
        tasks.push(retained_task(api, manifest).await);
    }
    tasks
}

fn all_tasks(api: &SupervisorApi, base: &TaskQuery) -> Vec<Task> {
    let mut query = base.clone();
    let mut result = Vec::new();
    let mut version = None;
    loop {
        let page = api.query_tasks(&query).expect("task snapshot page");
        if let Some(version) = &version {
            assert_eq!(version, &page.resource_version);
        } else {
            version = Some(page.resource_version.clone());
        }
        result.extend(page.items);
        match page.continuation {
            Some(cursor) => query = base.clone().with_continuation(cursor),
            None => return result,
        }
    }
}

fn all_runs(api: &SupervisorApi, name: &TaskId, base: &TaskRunQuery) -> Vec<TaskRun> {
    let mut query = base.clone();
    let mut result = Vec::new();
    let mut version = None;
    loop {
        let page = api
            .query_task_runs(name, &query)
            .expect("run snapshot page")
            .expect("retained task");
        if let Some(version) = &version {
            assert_eq!(version, &page.resource_version);
        } else {
            version = Some(page.resource_version.clone());
        }
        result.extend(page.items);
        match page.continuation {
            Some(cursor) => query = base.clone().with_continuation(cursor),
            None => return result,
        }
    }
}

async fn next(watch: &mut TaskWatchSubscription) -> TaskWatchEvent {
    bounded(watch.next())
        .await
        .expect("watch closed unexpectedly")
        .expect("watch position must be retained")
}

async fn next_generation(api: &SupervisorApi, generation: u64) -> Task {
    let task = bounded(api.apply_embedded_task(
        embedded_revision("history", "history", generation),
        immediate_task(),
    ))
    .await
    .expect("history generation apply");
    wait_task(api, task.name(), |task| {
        task.metadata().generation() == generation && task.phase() == &TaskPhase::Succeeded
    })
    .await;
    bounded(api.cancel_task(task.name()))
        .await
        .expect("history generation settlement");
    api.get_task(task.name())
        .expect("retained history resource")
}

fn bench_list(c: &mut Criterion) {
    print_suite_header("collections");
    let mut group = c.benchmark_group(LIST.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for (variant, count, filter, payload, byte_limit) in [
            ("32_all", 32, "all", 0, 4 * 1024 * 1024),
            ("128_all", 128, "all", 0, 4 * 1024 * 1024),
            ("128_slot", 128, "slot", 0, 4 * 1024 * 1024),
            ("128_labels_and_phase", 128, "labels", 0, 4 * 1024 * 1024),
            ("128_payload512_page4k", 128, "all", 512, 4096),
        ] {
            group.bench_function(BenchmarkId::new(runtime_name, variant), |b| {
                record_case(LIST, runtime_name, Some(variant.to_owned()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        populate(&api, count, payload).await;
                        let mut query = TaskQuery::new().with_limit(16).with_item_byte_limit(
                            NonZeroUsize::new(byte_limit).expect("positive page budget"),
                        );
                        let expected = match filter {
                            "slot" => {
                                query = query.with_slot(Slot::new("slot-0").expect("valid slot"));
                                count / 4
                            }
                            "labels" => {
                                query = query
                                    .with_phase(TaskPhase::Succeeded)
                                    .with_label_selector(
                                        "cohort=blue".parse().expect("valid selector"),
                                    )
                                    .expect("valid query");
                                count / 2
                            }
                            _ => count,
                        };
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let started = Instant::now();
                            let items = all_tasks(&api, &query);
                            total += started.elapsed();
                            assert_eq!(items.len(), expected);
                            assert!(items.windows(2).all(|pair| pair[0].name() < pair[1].name()));
                            black_box(items);
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

fn bench_interleaved_list(c: &mut Criterion) {
    let mut group = c.benchmark_group(INTERLEAVED.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for count in [32, 128] {
            let variant = format!("{count}_tasks_8_mutations");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(INTERLEAVED, runtime_name, Some(variant.clone()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        populate(&api, count, 0).await;
                        let query = TaskQuery::new().with_limit(8);
                        let mut total = Duration::ZERO;
                        for index in 0..iterations {
                            let expected = all_tasks(&api, &TaskQuery::new().with_limit(1000));
                            let requests = expected
                                .iter()
                                .rev()
                                .take(8)
                                .map(|task| {
                                    with_label(TaskManifest::from(task), &format!("cycle-{index}"))
                                })
                                .collect::<Vec<_>>();
                            let source = immediate_task();
                            let started = Instant::now();
                            let first = api.query_tasks(&query).expect("first snapshot page");
                            let version = first.resource_version;
                            let mut actual = first.items;
                            let mut continuation = first.continuation;
                            for request in requests {
                                bounded(api.apply_embedded_task(request, source.clone()))
                                    .await
                                    .expect("interleaved metadata apply");
                            }
                            while let Some(cursor) = continuation {
                                let page = api
                                    .query_tasks(&query.clone().with_continuation(cursor))
                                    .expect("historical snapshot page");
                                assert_eq!(page.resource_version, version);
                                actual.extend(page.items);
                                continuation = page.continuation;
                            }
                            total += started.elapsed();
                            assert_eq!(
                                actual, expected,
                                "live metadata must not leak into the captured snapshot"
                            );
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

fn bench_initial_watch(c: &mut Criterion) {
    let mut group = c.benchmark_group(INITIAL.group_id);
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for count in [32, 128] {
            group.throughput(Throughput::Elements(count as u64));
            let variant = format!("{count}_items");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(INITIAL, runtime_name, Some(variant.clone()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        let expected = populate(&api, count, 0).await;
                        let filter = TaskFilter::new();
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let mut names = Vec::with_capacity(count);
                            let started = Instant::now();
                            let mut watch =
                                api.watch_tasks(&filter, Some("0")).expect("initial watch");
                            for _ in 0..count {
                                match next(&mut watch).await {
                                    TaskWatchEvent::Added(task) => names.push(task.name().clone()),
                                    event => panic!("unexpected initial event: {event:?}"),
                                }
                            }
                            total += started.elapsed();
                            assert!(names.iter().eq(expected.iter().map(Task::name)));
                            drop(watch);
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

fn bench_replay_and_live(c: &mut Criterion) {
    const CHANGES: usize = 8;
    for family in [REPLAY, LIVE] {
        let mut group = c.benchmark_group(family.group_id);
        for &(runtime_name, runtime_factory) in &RUNTIMES {
            for watchers in [1_usize, 8] {
                if family.group_id == REPLAY.group_id && watchers != 1 {
                    continue;
                }
                let variant = format!("{watchers}_watchers_{CHANGES}_changes");
                group.throughput(Throughput::Elements((watchers * CHANGES) as u64));
                group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                    record_case(family, runtime_name, Some(variant.clone()));
                    let rt = runtime_factory();
                    b.iter_custom(|iterations| {
                        rt.block_on(async {
                            let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                                .await
                                .expect("SDK startup");
                            let stable =
                                retained_task(&api, embedded_manifest("watched", "watched")).await;
                            let original = TaskManifest::from(stable);
                            let filter = TaskFilter::new();
                            let source = immediate_task();
                            let mut total = Duration::ZERO;
                            for index in 0..iterations {
                                let baseline = api
                                    .query_tasks(&TaskQuery::new())
                                    .expect("baseline version")
                                    .resource_version;
                                let requests = (0..CHANGES)
                                    .map(|change| {
                                        with_label(
                                            original.clone(),
                                            &format!("cycle-{index}-{change}"),
                                        )
                                    })
                                    .collect::<Vec<_>>();
                                let replay = family.group_id == REPLAY.group_id;
                                let mut subscriptions = Vec::with_capacity(watchers);
                                if !replay {
                                    for _ in 0..watchers {
                                        subscriptions.push(
                                            api.watch_tasks(&filter, Some(&baseline))
                                                .expect("live watch"),
                                        );
                                    }
                                }
                                let mut expected = Vec::with_capacity(CHANGES);
                                let live_started = Instant::now();
                                for request in requests {
                                    let committed =
                                        bounded(api.apply_embedded_task(request, source.clone()))
                                            .await
                                            .expect("metadata commit");
                                    expected
                                        .push(committed.metadata().resource_version().to_owned());
                                }
                                let replay_started = Instant::now();
                                if replay {
                                    subscriptions.push(
                                        api.watch_tasks(&filter, Some(&baseline))
                                            .expect("replay watch"),
                                    );
                                }
                                for watch in &mut subscriptions {
                                    for version in &expected {
                                        match next(watch).await {
                                            TaskWatchEvent::Modified(task) => assert_eq!(
                                                task.metadata().resource_version(),
                                                version
                                            ),
                                            event => {
                                                panic!("unexpected metadata watch event: {event:?}")
                                            }
                                        }
                                    }
                                }
                                total += if replay {
                                    replay_started.elapsed()
                                } else {
                                    live_started.elapsed()
                                };
                                drop(subscriptions);
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
}

fn bench_watch_limits(c: &mut Criterion) {
    for family in [EXPIRED, WATCH_ADMISSION] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(runtime_name, runtime_factory) in &RUNTIMES {
            for live in [false, true] {
                if family.group_id == WATCH_ADMISSION.group_id && live {
                    continue;
                }
                let variant = if family.group_id == WATCH_ADMISSION.group_id {
                    "count_1"
                } else if live {
                    "slow_live"
                } else {
                    "resume"
                };
                group.bench_function(BenchmarkId::new(runtime_name, variant), |b| {
                    record_case(family, runtime_name, Some(variant.to_owned()));
                    let rt = runtime_factory();
                    b.iter_custom(|iterations| {
                        rt.block_on(async {
                            let config = StateConfig::new()
                                .try_with_watch_history_capacity(8)
                                .expect("journal limit")
                                .try_with_max_concurrent_task_watches(1)
                                .expect("watch limit");
                            let api = bounded(
                                SupervisorApi::builder(RunnerRouter::new())
                                    .with_state_config(config)
                                    .start(),
                            )
                            .await
                            .expect("SDK startup");
                            let stable = retained_task(
                                &api,
                                embedded_manifest("watch-limit", "watch-limit"),
                            )
                            .await;
                            let manifest = TaskManifest::from(stable);
                            let filter = TaskFilter::new();
                            let source = immediate_task();
                            let mut total = Duration::ZERO;
                            for index in 0..iterations {
                                let baseline = api
                                    .query_tasks(&TaskQuery::new())
                                    .expect("baseline version")
                                    .resource_version;
                                let mut occupied =
                                    if live || family.group_id == WATCH_ADMISSION.group_id {
                                        Some(
                                            api.watch_tasks(&filter, Some(&baseline))
                                                .expect("occupied watch"),
                                        )
                                    } else {
                                        None
                                    };
                                if family.group_id == EXPIRED.group_id {
                                    for change in 0..16 {
                                        let request = with_label(
                                            manifest.clone(),
                                            &format!("cycle-{index}-{change}"),
                                        );
                                        bounded(api.apply_embedded_task(request, source.clone()))
                                            .await
                                            .expect("journal-overflow mutation");
                                    }
                                }
                                let started = Instant::now();
                                if family.group_id == WATCH_ADMISSION.group_id {
                                    assert!(matches!(
                                        api.watch_tasks(&filter, Some(&baseline)),
                                        Err(
                                            CollectionError::ConcurrentTaskWatchLimitReached { .. }
                                        )
                                    ));
                                } else if let Some(watch) = occupied.as_mut() {
                                    assert!(matches!(
                                        bounded(watch.next()).await,
                                        Some(Err(CollectionError::ResourceVersionExpired { .. }))
                                    ));
                                } else {
                                    assert!(matches!(
                                        api.watch_tasks(&filter, Some(&baseline)),
                                        Err(CollectionError::ResourceVersionExpired { .. })
                                    ));
                                }
                                total += started.elapsed();
                                if live {
                                    assert!(
                                        bounded(occupied.as_mut().expect("live watch").next())
                                            .await
                                            .is_none()
                                    );
                                }
                                drop(occupied);
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
}

fn bench_run_history(c: &mut Criterion) {
    let mut group = c.benchmark_group(RUNS.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for generations in [8_u64, 32] {
            let variant = format!("{generations}_generations");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(RUNS, runtime_name, Some(variant.clone()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        let first =
                            retained_task(&api, embedded_revision("history", "history", 1)).await;
                        for generation in 2..=generations {
                            next_generation(&api, generation).await;
                        }
                        let query = TaskRunQuery::new().with_limit(4);
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let started = Instant::now();
                            let runs = all_runs(&api, first.name(), &query);
                            total += started.elapsed();
                            assert_eq!(runs.len(), generations as usize);
                            assert!(runs.iter().map(TaskRun::generation).eq(1..=generations));
                            black_box(runs);
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

fn bench_run_cap(c: &mut Criterion) {
    let mut group = c.benchmark_group(RUN_CAP.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        group.bench_function(runtime_name, |b| {
            record_case(RUN_CAP, runtime_name, None);
            let rt = runtime_factory();
            b.iter_custom(|iterations| {
                rt.block_on(async {
                    let api = bounded(
                        SupervisorApi::builder(RunnerRouter::new())
                            .with_state_config(StateConfig::new().with_max_runs_per_task(4))
                            .start(),
                    )
                    .await
                    .expect("SDK startup");
                    let first_task =
                        retained_task(&api, embedded_revision("history", "history", 1)).await;
                    for generation in 2..=4 {
                        next_generation(&api, generation).await;
                    }
                    let query = TaskRunQuery::new().with_limit(1);
                    let mut total = Duration::ZERO;
                    for index in 0..iterations {
                        let expected = all_runs(&api, first_task.name(), &TaskRunQuery::new());
                        let generation = index + 5;
                        let request = embedded_revision("history", "history", generation);
                        let source = immediate_task();
                        let started = Instant::now();
                        let first = api
                            .query_task_runs(first_task.name(), &query)
                            .expect("first run page")
                            .expect("history resource");
                        let version = first.resource_version;
                        let mut actual = first.items;
                        let mut continuation = first.continuation;
                        bounded(api.apply_embedded_task(request, source))
                            .await
                            .expect("next history generation");
                        wait_task(&api, first_task.name(), |task| {
                            task.metadata().generation() == generation
                                && task.phase() == &TaskPhase::Succeeded
                        })
                        .await;
                        bounded(api.cancel_task(first_task.name()))
                            .await
                            .expect("history settlement");
                        while let Some(cursor) = continuation {
                            let page = api
                                .query_task_runs(
                                    first_task.name(),
                                    &query.clone().with_continuation(cursor),
                                )
                                .expect("historical run continuation")
                                .expect("history resource");
                            assert_eq!(page.resource_version, version);
                            actual.extend(page.items);
                            continuation = page.continuation;
                        }
                        total += started.elapsed();
                        assert_eq!(actual, expected);
                        let current = all_runs(&api, first_task.name(), &TaskRunQuery::new());
                        assert_eq!(current.len(), 4);
                        assert_eq!(
                            current.first().expect("four retained runs").generation(),
                            generation - 3
                        );
                        assert_eq!(
                            current.last().expect("four retained runs").generation(),
                            generation
                        );
                    }
                    bounded(api.shutdown()).await.expect("SDK shutdown");
                    total
                })
            });
        });
    }
    group.finish();
}

#[cfg(feature = "fixtures")]
fn bench_retention(c: &mut Criterion) {
    let mut group = c.benchmark_group(SWEEP.group_id);
    for &(runtime_name, runtime_factory) in &RUNTIMES {
        for count in [8_usize, 32] {
            group.throughput(Throughput::Elements(count as u64));
            let variant = format!("{count}_terminal_resources");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(SWEEP, runtime_name, Some(variant.clone()));
                let rt = runtime_factory();
                b.iter_custom(|iterations| {
                    rt.block_on(async {
                        let api = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
                            .await
                            .expect("SDK startup");
                        let state = api.state();
                        let expired = StateConfig::new()
                            .with_run_ttl(Duration::ZERO)
                            .with_task_ttl(Duration::ZERO);
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            populate(&api, count, 0).await;
                            let started = Instant::now();
                            let removed = state.sweep_retention_for_test(&expired);
                            total += started.elapsed();
                            assert_eq!(removed, (count, count));
                            assert!(
                                api.query_tasks(&TaskQuery::new())
                                    .expect("post-sweep query")
                                    .items
                                    .is_empty()
                            );
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

#[cfg(not(feature = "fixtures"))]
fn bench_retention(_c: &mut Criterion) {}

criterion_group!(
    benches,
    bench_list,
    bench_interleaved_list,
    bench_initial_watch,
    bench_replay_and_live,
    bench_watch_limits,
    bench_run_history,
    bench_run_cap,
    bench_retention
);

fn main() {
    benchmark_main("collections", benches);
}
