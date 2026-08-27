//! Real subprocess output through core's live output subscriptions.

#[path = "support/process.rs"]
mod process;

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use futures_util::StreamExt as _;
use solti_benches::{
    fixtures::{RUNTIMES, WAIT_BOUND},
    report::{CaseFamily, benchmark_main, print_suite_header, record_case},
};
use solti_core::{OutputConfig, OutputSubscription, SupervisorApi};
use solti_exec::subprocess::{SubprocessBackendConfig, register_subprocess_runner_with_backend};
use solti_model::{OutputEvent, TaskPhase};
use solti_runner::RunnerRouter;

const LINES: usize = 128;
const FANOUT: CaseFamily = CaseFamily::lifecycle(
    "output/fanout", "REAL OUTPUT · LIVE FANOUT", "source line", "source lines",
    "release of a ready real subprocess through output drain, terminal core state, all subscriber RunFinished events and released process ownership",
    "runtime, API, runner, manifest, process startup and subscriptions; final API/runner shutdown; source-line rate is not subscriber-delivery rate",
).without_lifecycle_interpretation();
const PRESSURE: CaseFamily = CaseFamily::lifecycle(
    "output/pressure", "REAL OUTPUT · BOUNDED LOSS", "offered source line", "offered source lines",
    "release of a ready real subprocess through terminal state, delayed subscriber drain through RunFinished and released process ownership",
    "runtime, API, runner, task startup and subscription; final shutdown; lag/truncation assertions; rate counts offered lines including loss",
).without_lifecycle_interpretation();

#[derive(Clone, Copy)]
struct Case {
    label: &'static str,
    subscribers: usize,
    width: usize,
    event_capacity: usize,
    chunk_limit: usize,
    aggregate: usize,
    delayed: bool,
    expect_loss: bool,
}

const CASES: &[Case] = &[
    Case {
        label: "none_64b",
        subscribers: 0,
        width: 64,
        event_capacity: 512,
        chunk_limit: 4096,
        aggregate: 8 * 1024 * 1024,
        delayed: false,
        expect_loss: false,
    },
    Case {
        label: "one_64b",
        subscribers: 1,
        width: 64,
        event_capacity: 512,
        chunk_limit: 4096,
        aggregate: 8 * 1024 * 1024,
        delayed: false,
        expect_loss: false,
    },
    Case {
        label: "four_64b",
        subscribers: 4,
        width: 64,
        event_capacity: 512,
        chunk_limit: 4096,
        aggregate: 8 * 1024 * 1024,
        delayed: false,
        expect_loss: false,
    },
    Case {
        label: "eight_64b",
        subscribers: 8,
        width: 64,
        event_capacity: 512,
        chunk_limit: 4096,
        aggregate: 8 * 1024 * 1024,
        delayed: false,
        expect_loss: false,
    },
    Case {
        label: "oversized_8kib",
        subscribers: 1,
        width: 8192,
        event_capacity: 512,
        chunk_limit: 128,
        aggregate: 8 * 1024 * 1024,
        delayed: true,
        expect_loss: false,
    },
    Case {
        label: "slow_event_ring8",
        subscribers: 1,
        width: 64,
        event_capacity: 8,
        chunk_limit: 128,
        aggregate: 8 * 1024 * 1024,
        delayed: true,
        expect_loss: true,
    },
    Case {
        label: "slow_aggregate128b",
        subscribers: 1,
        width: 64,
        event_capacity: 512,
        chunk_limit: 128,
        aggregate: 128,
        delayed: true,
        expect_loss: true,
    },
];

#[derive(Default, Debug)]
struct Observed {
    chunks: u64,
    bytes: u64,
    truncated: u64,
    skipped: u64,
    skipped_bytes: u64,
}

async fn consume(mut output: OutputSubscription) -> Observed {
    tokio::time::timeout(WAIT_BOUND, async {
        let mut observed = Observed::default();
        while let Some(event) = output.next().await {
            match event {
                OutputEvent::Chunk(chunk) => {
                    observed.chunks += 1;
                    observed.bytes += chunk.line.len() as u64;
                    observed.truncated += u64::from(chunk.truncated);
                }
                OutputEvent::Lagged {
                    skipped,
                    skipped_bytes,
                } => {
                    observed.skipped += skipped;
                    observed.skipped_bytes += skipped_bytes;
                }
                OutputEvent::RunFinished { .. } => return observed,
                _ => {}
            }
        }
        panic!("output channel closed without RunFinished");
    })
    .await
    .expect("output consumer deadline")
}

async fn terminal(api: &SupervisorApi, name: &solti_model::TaskId) {
    tokio::time::timeout(WAIT_BOUND, async {
        loop {
            let task = api.get_task(name).expect("output task exists");
            if task.phase().is_terminal() {
                assert_eq!(*task.phase(), TaskPhase::Succeeded);
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("output task terminal deadline");
}

fn bench_output(c: &mut Criterion) {
    print_suite_header("output");
    for family in [FANOUT, PRESSURE] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(LINES as u64));
        for &(rt_name, rt_fn) in &RUNTIMES {
            for &case in CASES {
                if (family.group_id == PRESSURE.group_id) != case.delayed {
                    continue;
                }
                group.bench_function(BenchmarkId::new(rt_name, case.label), |b| {
                    record_case(family, rt_name, Some(case.label.to_owned()));
                    let rt = rt_fn();
                    b.iter_custom(|iters| {
                        rt.block_on(async {
                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let gate = process::Gate::new();
                                let mut args = gate.args("output");
                                args.extend([LINES.to_string(), case.width.to_string()]);
                                let mut router = RunnerRouter::new();
                                let runner = register_subprocess_runner_with_backend(
                                    &mut router,
                                    "output-process",
                                    SubprocessBackendConfig::new(),
                                )
                                .expect("output runner");
                                let config = OutputConfig::try_new(case.event_capacity)
                                    .unwrap()
                                    .try_with_byte_limits(
                                        case.event_capacity * case.chunk_limit,
                                        case.chunk_limit,
                                    )
                                    .unwrap()
                                    .try_with_aggregate_byte_budget(case.aggregate)
                                    .unwrap();
                                let api = SupervisorApi::builder(router)
                                    .with_output_config(config)
                                    .start()
                                    .await
                                    .expect("output core API");
                                let task = api
                                    .create_task(process::manifest(
                                        "output-task",
                                        process::command_workload(args),
                                    ))
                                    .await
                                    .expect("output task");
                                gate.wait_ready().await;
                                let subscriptions = (0..case.subscribers)
                                    .map(|_| {
                                        api.subscribe_output(task.name())
                                            .expect("live output subscription")
                                    })
                                    .collect::<Vec<_>>();
                                let mut consumers = Vec::new();
                                let mut delayed = Vec::new();
                                for subscription in subscriptions {
                                    if case.delayed {
                                        delayed.push(subscription);
                                    } else {
                                        consumers.push(tokio::spawn(consume(subscription)));
                                    }
                                }
                                let start = Instant::now();
                                gate.open();
                                terminal(&api, task.name()).await;
                                let mut observed = Vec::new();
                                for consumer in consumers {
                                    observed.push(consumer.await.expect("live output consumer"));
                                }
                                for subscription in delayed {
                                    observed.push(consume(subscription).await);
                                }
                                process::wait_clean(&runner).await;
                                total += start.elapsed();
                                for stats in observed {
                                    if case.expect_loss {
                                        assert!(
                                            stats.skipped > 0,
                                            "expected observable output lag: {stats:?}"
                                        );
                                        assert!(
                                            stats.skipped_bytes > 0,
                                            "expected observable byte loss: {stats:?}"
                                        );
                                        assert!(stats.chunks < LINES as u64);
                                    } else {
                                        assert_eq!(stats.chunks, LINES as u64);
                                        assert_eq!(
                                            stats.bytes,
                                            (LINES * case.width.min(4096).min(case.chunk_limit))
                                                as u64
                                        );
                                        assert_eq!(stats.skipped, 0);
                                        assert_eq!(
                                            stats.truncated,
                                            if case.width > case.chunk_limit.min(4096) {
                                                LINES as u64
                                            } else {
                                                0
                                            }
                                        );
                                    }
                                    std::hint::black_box(stats);
                                }
                                api.shutdown().await.expect("output API shutdown");
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
}

criterion_group!(benches, bench_output);

fn main() {
    if !process::maybe_child() {
        benchmark_main("output", benches);
    }
}
