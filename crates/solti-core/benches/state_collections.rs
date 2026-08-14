//! Release-mode baselines for bounded Task collection operations.

use std::{hint::black_box, time::Instant};

use solti_core::TaskState;
use solti_model::{EmbeddedSpec, TaskFilter, TaskId, TaskQuery, TaskSpec, TaskWorkload};

const TASKS: usize = 1_024;
const WARMUP_ITERATIONS: usize = 20;
const MEASURED_ITERATIONS: usize = 200;

fn main() {
    let state = populated_state();
    let query = TaskQuery::new().with_limit(1_000);

    measure("query first 1000 of 1024 Tasks", || {
        black_box(state.query(black_box(&query)).expect("benchmark query"));
    });
    measure("capture 1024-Task watch snapshot", || {
        drop(black_box(
            state
                .watch(black_box(&TaskFilter::new()), None)
                .expect("benchmark watch"),
        ));
    });
}

fn populated_state() -> TaskState {
    let state = TaskState::new();
    let spec = TaskSpec::builder(
        "benchmark",
        TaskWorkload::Embedded(EmbeddedSpec::new("benchmark-v1").expect("embedded kind")),
        30_000_u64,
    )
    .build()
    .expect("benchmark task spec");

    for index in 0..TASKS {
        let name =
            TaskId::new(format!("benchmark-{index:04}")).expect("generated benchmark Task name");
        state.seed_task(name, spec.clone());
    }
    state
}

fn measure(label: &str, mut operation: impl FnMut()) {
    for _ in 0..WARMUP_ITERATIONS {
        operation();
    }

    let started = Instant::now();
    for _ in 0..MEASURED_ITERATIONS {
        operation();
    }
    let elapsed = started.elapsed();
    let nanos_per_operation = elapsed.as_nanos() / MEASURED_ITERATIONS as u128;
    println!("{label}: {nanos_per_operation} ns/op ({MEASURED_ITERATIONS} iterations)");
}
