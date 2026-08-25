//! # Bounded state collection benchmarks
//!
//! Measures retained-task lookup, bounded snapshot queries, watch snapshot
//! construction, and an immediate retention pass over a fixed 1,024-task
//! fixture.
//!
//! Fixture construction is outside every measured boundary. Query and watch
//! cases include result validation. The retention case measures only the sweep;
//! creating terminal Tasks is per-iteration setup.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use solti_core::{StateConfig, TaskState};
use solti_model::{
    EmbeddedSpec, Slot, TaskFilter, TaskId, TaskPhase, TaskQuery, TaskSpec, TaskWorkload,
};

const TASKS: usize = 1_024;
const INDEXED_TASKS: usize = TASKS / 2;
const MEASUREMENT_TIME: Duration = Duration::from_secs(10);
const SAMPLE_SIZE: usize = 30;

fn bench_bounded_queries(c: &mut Criterion) {
    let state = populated_state();
    let mut group = c.benchmark_group("state_collections/query");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(MEASUREMENT_TIME);

    for limit in [100, 1_000] {
        let query = TaskQuery::new().with_limit(limit);
        group.throughput(Throughput::Elements(limit as u64));
        group.bench_with_input(
            BenchmarkId::new("first_page", format!("{limit}_of_{TASKS}")),
            &query,
            |b, query| {
                b.iter(|| {
                    let page = state.query(black_box(query)).expect("benchmark query");
                    assert_eq!(page.items.len(), limit);
                    assert_eq!(page.remaining_item_count, TASKS - limit);
                    black_box(page);
                });
            },
        );
    }

    let indexed_query = TaskQuery::new()
        .with_slot(Slot::new("benchmark-even").expect("benchmark slot"))
        .with_limit(INDEXED_TASKS);
    group.throughput(Throughput::Elements(INDEXED_TASKS as u64));
    group.bench_function("indexed_slot/512_of_1024", |b| {
        b.iter(|| {
            let page = state
                .query(black_box(&indexed_query))
                .expect("benchmark indexed query");
            assert_eq!(page.items.len(), INDEXED_TASKS);
            assert_eq!(page.remaining_item_count, 0);
            black_box(page);
        });
    });
    group.finish();
}

fn bench_bounded_list(c: &mut Criterion) {
    let state = populated_state();
    let mut group = c.benchmark_group("state_collections/list");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(MEASUREMENT_TIME);
    group.throughput(Throughput::Elements(INDEXED_TASKS as u64));
    group.bench_function("slot/512_of_1024", |b| {
        b.iter(|| {
            let tasks = state.list_by_slot(black_box("benchmark-even"));
            assert_eq!(tasks.len(), INDEXED_TASKS);
            black_box(tasks);
        });
    });
    group.finish();
}

fn bench_watch_snapshot(c: &mut Criterion) {
    let state = populated_state();
    let filter = TaskFilter::new();
    let mut group = c.benchmark_group("state_collections/watch");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(MEASUREMENT_TIME);
    group.throughput(Throughput::Elements(TASKS as u64));
    group.bench_function("construct_and_drop_snapshot/1024", |b| {
        b.iter(|| {
            let subscription = state
                .watch(black_box(&filter), None)
                .expect("benchmark watch");
            drop(black_box(subscription));
        });
    });
    group.finish();
}

fn bench_retention_sweep(c: &mut Criterion) {
    let config = StateConfig::new()
        .with_run_ttl(Duration::ZERO)
        .with_task_ttl(Duration::ZERO);
    let mut group = c.benchmark_group("state_collections/retention");
    group.sample_size(SAMPLE_SIZE);
    group.measurement_time(MEASUREMENT_TIME);
    group.throughput(Throughput::Elements(TASKS as u64));
    group.bench_function("remove_1024_terminal_runs", |b| {
        b.iter_batched(
            terminal_state,
            |state| {
                let removed = state.sweep_retention_for_test(black_box(&config));
                assert_eq!(removed, (TASKS, 0));
                black_box(removed);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn populated_state() -> TaskState {
    let state = TaskState::new();
    let even = task_spec("benchmark-even");
    let odd = task_spec("benchmark-odd");

    for index in 0..TASKS {
        let name = task_id(index);
        let spec = if index % 2 == 0 { &even } else { &odd };
        state.seed_task(name, spec.clone());
    }
    state
}

fn terminal_state() -> TaskState {
    let state = TaskState::new();
    let spec = task_spec("benchmark-retention");
    for index in 0..TASKS {
        let name = task_id(index);
        state.seed_task(name.clone(), spec.clone());
        state.seed_finished(&name, TaskPhase::Succeeded, None, Some(0));
    }
    state
}

fn task_spec(slot: &str) -> TaskSpec {
    TaskSpec::builder(
        slot,
        TaskWorkload::Embedded(EmbeddedSpec::new("benchmark-v1").expect("embedded kind")),
        30_000_u64,
    )
    .build()
    .expect("benchmark Task spec")
}

fn task_id(index: usize) -> TaskId {
    TaskId::new(format!("benchmark-{index:04}")).expect("generated benchmark Task name")
}

criterion_group!(
    benches,
    bench_bounded_queries,
    bench_bounded_list,
    bench_watch_snapshot,
    bench_retention_sweep
);
criterion_main!(benches);
