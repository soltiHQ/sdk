//! Real loopback HTTP around the public adapter and a controlled extension runner.

#[path = "boundary_support/http.rs"]
mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use serde_json::Value;
use solti_benches::fixtures::{RUNTIMES, bounded, wait_task, wait_terminal};
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};
use solti_model::{Labels, OutputEvent, StreamKind, Task, TaskId, TaskPhase};
use tokio::sync::Semaphore;

use support::{FramedBody, HttpFixture, RunnerOptions, manifest};

const COMMIT: CaseFamily = CaseFamily::intake(
    "http/steady/create_commit",
    "HTTP · CREATE THROUGH DESIRED-STATE COMMIT",
    "committed request",
    "committed requests",
    "POST Queue manifest over loopback through complete HTTP 201 body deserialization",
    "runtime/server/core/client/manifest setup, connection and one-task warm-up, task completion and deletion",
);
const COMPLETE: CaseFamily = CaseFamily::lifecycle(
    "http/steady/create_observe_success",
    "HTTP · CREATE THROUGH CLIENT-OBSERVED SUCCESS",
    "observed task",
    "observed tasks",
    "POST Queue manifest, reconciliation and controlled task execution, repeated GET until client decodes Succeeded",
    "runtime/server/core/client/manifest setup, connection and one-task warm-up, deletion; GET polling cost is inside",
).without_lifecycle_interpretation();
const PAGINATION: CaseFamily = CaseFamily::query(
    "http/steady/snapshot_walk",
    "HTTP · COMPLETE SNAPSHOT WALK",
    "returned Task",
    "returned Tasks",
    "first page through every continuation page, complete JSON decode and snapshot/uniqueness checks",
    "population and terminal observation, connection setup, writer join/cleanup; writes start after first page",
);
const INITIAL_WATCH: CaseFamily = CaseFamily::query(
    "http/steady/watch_initial",
    "HTTP · INITIAL NDJSON WATCH SNAPSHOT",
    "decoded event",
    "decoded events",
    "open HTTP watch through all initial Added documents, including incremental NDJSON decoding",
    "population and terminal observation, server/client/runtime setup and stream close",
);
const REPLAY_WATCH: CaseFamily = CaseFamily::query(
    "http/steady/watch_replay",
    "HTTP · NDJSON WATCH REPLAY",
    "decoded event",
    "decoded events",
    "resume HTTP watch at saved resourceVersion through all retained metadata-change documents",
    "population, saved resourceVersion read, metadata writes, server/client/runtime setup and stream close",
);
const LIVE_WATCH: CaseFamily = CaseFamily::lifecycle(
    "http/steady/watch_live_completion",
    "HTTP · SUBMISSION THROUGH LIVE WATCH COMPLETION",
    "observed task",
    "observed tasks",
    "HTTP POST Queue batch through decoding a live Succeeded watch document for every submitted Task",
    "runtime/server/client setup, one-task warm-up, opened watch, stream close and task deletion",
)
.without_lifecycle_interpretation();
const OUTPUT: CaseFamily = CaseFamily::query(
    "http/steady/output_sse",
    "HTTP · RUNNER OUTPUT THROUGH SSE CLIENT",
    "decoded chunk",
    "decoded chunks",
    "release the running task, publish stdout/stderr, encode SSE, decode and verify every output chunk",
    "Queue task creation/start, opened subscription, payload construction, terminal observation and deletion",
);

fn writes(c: &mut Criterion) {
    print_suite_header("http");
    for (family, observe) in [(COMMIT, false), (COMPLETE, true)] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(runtime_name, make_runtime) in &RUNTIMES {
            for auth in [false, true] {
                let variant = if auth { "bearer" } else { "no_auth" };
                group.bench_function(BenchmarkId::new(runtime_name, variant), |b| {
                    record_case(family, runtime_name, Some(variant.into()));
                    let runtime = make_runtime();
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let fixture = HttpFixture::new(RunnerOptions::default(), auth).await;
                            let manifest = manifest("write-probe", 128);
                            let warm = fixture.create(&manifest).await;
                            fixture.observed_success(warm.name()).await;
                            fixture.delete(warm.name()).await;
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let start = Instant::now();
                                let created = fixture.create(&manifest).await;
                                if observe {
                                    fixture.observed_success(created.name()).await;
                                }
                                total += start.elapsed();
                                assert_eq!(created.name(), manifest.name());
                                if !observe {
                                    assert_eq!(
                                        wait_terminal(&fixture.supervisor, created.name())
                                            .await
                                            .phase(),
                                        &TaskPhase::Succeeded
                                    );
                                }
                                fixture.delete(created.name()).await;
                            }
                            fixture.close().await;
                            total
                        })
                    });
                });
            }
        }
        group.finish();
    }
}

async fn populate(fixture: &HttpFixture, count: usize) -> Vec<TaskId> {
    let mut names = Vec::with_capacity(count);
    for index in 0..count {
        let task = bounded(
            fixture
                .supervisor
                .create_task(manifest(&format!("base-{index:04}"), 128)),
        )
        .await
        .unwrap();
        names.push(task.name().clone());
    }
    for name in &names {
        assert_eq!(
            wait_terminal(&fixture.supervisor, name).await.phase(),
            &TaskPhase::Succeeded
        );
    }
    names
}

async fn page(fixture: &HttpFixture, limit: usize, continuation: &str) -> Value {
    let mut query = vec![("limit", limit.to_string())];
    if !continuation.is_empty() {
        query.push(("continue", continuation.to_owned()));
    }
    let response = bounded(fixture.client.get(&fixture.tasks_url).query(&query).send())
        .await
        .unwrap();
    assert!(response.status().is_success());
    bounded(response.json()).await.unwrap()
}

fn pagination(c: &mut Criterion) {
    let mut group = c.benchmark_group(PAGINATION.group_id);
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for (count, page_size) in [(64, 16), (256, 32)] {
            for concurrent_writes in [false, true] {
                let variant = format!(
                    "{count}_tasks/page_{page_size}/{}",
                    if concurrent_writes {
                        "writes"
                    } else {
                        "stable"
                    }
                );
                group.throughput(Throughput::Elements(count as u64));
                group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                    record_case(PAGINATION, runtime_name, Some(variant.clone()));
                    let runtime = make_runtime();
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let fixture = HttpFixture::new(RunnerOptions::default(), false).await;
                            let expected: BTreeSet<_> = populate(&fixture, count)
                                .await
                                .into_iter()
                                .map(|name| name.to_string())
                                .collect();
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let start = Instant::now();
                                let mut current = page(&fixture, page_size, "").await;
                                let version = current["metadata"]["resourceVersion"]
                                    .as_str()
                                    .unwrap()
                                    .to_owned();
                                let writer = if concurrent_writes {
                                    let client = fixture.client.clone();
                                    let url = fixture.tasks_url.clone();
                                    Some(tokio::spawn(async move {
                                        let mut names = Vec::new();
                                        for index in 0..8 {
                                            let request = manifest(&format!("later-{index}"), 128);
                                            let response =
                                                bounded(client.post(&url).json(&request).send())
                                                    .await
                                                    .unwrap();
                                            assert_eq!(
                                                response.status(),
                                                reqwest::StatusCode::CREATED
                                            );
                                            let task: Task =
                                                bounded(response.json()).await.unwrap();
                                            names.push(task.name().clone());
                                        }
                                        names
                                    }))
                                } else {
                                    None
                                };
                                let mut seen = BTreeSet::new();
                                loop {
                                    assert_eq!(current["metadata"]["resourceVersion"], version);
                                    for item in current["items"].as_array().unwrap() {
                                        assert!(
                                            seen.insert(
                                                item["metadata"]["name"]
                                                    .as_str()
                                                    .unwrap()
                                                    .to_owned()
                                            ),
                                            "duplicate snapshot task"
                                        );
                                    }
                                    let continuation =
                                        current["metadata"]["continue"].as_str().unwrap_or("");
                                    if continuation.is_empty() {
                                        break;
                                    }
                                    current = page(&fixture, page_size, continuation).await;
                                }
                                assert_eq!(
                                    seen, expected,
                                    "snapshot changed while walking its pages"
                                );
                                total += start.elapsed();
                                if let Some(writer) = writer {
                                    for name in bounded(writer).await.unwrap() {
                                        wait_terminal(&fixture.supervisor, &name).await;
                                        fixture.delete(&name).await;
                                    }
                                }
                            }
                            fixture.close().await;
                            total
                        })
                    });
                });
            }
        }
    }
    group.finish();
}

fn initial_and_replay(c: &mut Criterion) {
    for (family, replay) in [(INITIAL_WATCH, false), (REPLAY_WATCH, true)] {
        let mut group = c.benchmark_group(family.group_id);
        for &(runtime_name, make_runtime) in &RUNTIMES {
            for count in [16, 64] {
                let variant = format!("{count}_events");
                group.throughput(Throughput::Elements(count as u64));
                group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                    record_case(family, runtime_name, Some(variant.clone()));
                    let runtime = make_runtime();
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let fixture = HttpFixture::new(RunnerOptions::default(), false).await;
                            let names = populate(&fixture, count).await;
                            let mut total = Duration::ZERO;
                            for iteration in 0..iterations {
                                let version = if replay {
                                    let snapshot = page(&fixture, 1, "").await;
                                    let version = snapshot["metadata"]["resourceVersion"]
                                        .as_str()
                                        .unwrap()
                                        .to_owned();
                                    for name in &names {
                                        let mut labels = Labels::new();
                                        labels.insert("dataset", "benchmark");
                                        labels.insert("revision", (iteration + 1).to_string());
                                        let update = manifest(name.as_str(), 128)
                                            .with_labels(labels)
                                            .unwrap();
                                        let task = bounded(fixture.supervisor.apply_task(update))
                                            .await
                                            .unwrap();
                                        assert_eq!(
                                            task.metadata().generation(),
                                            1,
                                            "metadata-only update changed desired generation"
                                        );
                                        assert_eq!(task.phase(), &TaskPhase::Succeeded);
                                    }
                                    version
                                } else {
                                    "0".to_owned()
                                };
                                let start = Instant::now();
                                let response = bounded(
                                    fixture
                                        .client
                                        .get(&fixture.tasks_url)
                                        .query(&[("watch", "true"), ("resourceVersion", &version)])
                                        .send(),
                                )
                                .await
                                .unwrap();
                                let mut stream = FramedBody::new(response, "application/json");
                                let mut seen = BTreeSet::new();
                                for _ in 0..count {
                                    let event = stream.ndjson().await;
                                    assert_eq!(
                                        event["type"],
                                        if replay { "MODIFIED" } else { "ADDED" }
                                    );
                                    let name = event["object"]["metadata"]["name"]
                                        .as_str()
                                        .unwrap()
                                        .to_owned();
                                    assert!(seen.insert(name));
                                }
                                assert_eq!(seen.len(), count);
                                total += start.elapsed();
                                drop(stream);
                            }
                            fixture.close().await;
                            total
                        })
                    });
                });
            }
        }
        group.finish();
    }
}

fn live_watch(c: &mut Criterion) {
    let mut group = c.benchmark_group(LIVE_WATCH.group_id);
    const COUNT: usize = 16;
    group.throughput(Throughput::Elements(COUNT as u64));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        group.bench_function(runtime_name, |b| {
            record_case(LIVE_WATCH, runtime_name, None);
            let runtime = make_runtime();
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let fixture = HttpFixture::new(RunnerOptions::default(), false).await;
                    let requests: Vec<_> = (0..COUNT)
                        .map(|index| manifest(&format!("live-{index}"), 128))
                        .collect();
                    let warm = fixture.create(&manifest("warm-live", 128)).await;
                    fixture.observed_success(warm.name()).await;
                    fixture.delete(warm.name()).await;
                    let mut total = Duration::ZERO;
                    for _ in 0..iterations {
                        let response = bounded(
                            fixture
                                .client
                                .get(&fixture.tasks_url)
                                .query(&[("watch", "true")])
                                .send(),
                        )
                        .await
                        .unwrap();
                        let mut stream = FramedBody::new(response, "application/json");
                        let start = Instant::now();
                        for request in &requests {
                            fixture.create(request).await;
                        }
                        let mut completed = BTreeSet::new();
                        while completed.len() < COUNT {
                            let event = stream.ndjson().await;
                            assert_ne!(event["type"], "ERROR", "watch failed: {event}");
                            let task: Task =
                                serde_json::from_value(event["object"].clone()).unwrap();
                            if task.phase() == &TaskPhase::Succeeded {
                                completed.insert(task.name().clone());
                            }
                        }
                        for request in &requests {
                            assert!(completed.contains(request.name()));
                        }
                        total += start.elapsed();
                        drop(stream);
                        for request in &requests {
                            fixture.delete(request.name()).await;
                        }
                    }
                    fixture.close().await;
                    total
                })
            });
        });
    }
    group.finish();
}

fn output(c: &mut Criterion) {
    let mut group = c.benchmark_group(OUTPUT.group_id);
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for (chunks, chunk_bytes) in [(64, 128), (256, 1_024)] {
            let variant = format!("{chunks}_chunks/{chunk_bytes}_bytes");
            group.throughput(Throughput::Elements(chunks as u64));
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(OUTPUT, runtime_name, Some(variant.clone()));
                let runtime = make_runtime();
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let gate = Arc::new(Semaphore::new(0));
                        let fixture = HttpFixture::new(
                            RunnerOptions {
                                gate: Some(Arc::clone(&gate)),
                                chunks,
                                chunk_bytes,
                            },
                            false,
                        )
                        .await;
                        let request = manifest("output-probe", 64);
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let created = fixture.create(&request).await;
                            wait_task(&fixture.supervisor, created.name(), |task| {
                                task.phase() == &TaskPhase::Running
                            })
                            .await;
                            let response = bounded(
                                fixture
                                    .client
                                    .get(format!("{}/{}/logs", fixture.tasks_url, created.name()))
                                    .query(&[("taskUid", created.uid().as_str())])
                                    .send(),
                            )
                            .await
                            .unwrap();
                            let mut stream = FramedBody::new(response, "text/event-stream");
                            let start = Instant::now();
                            gate.add_permits(1);
                            let mut received = 0;
                            let mut sequences = [0_u64; 2];
                            while received < chunks {
                                let mut event = stream.sse().await;
                                assert_eq!(event["taskUid"], created.uid().as_str());
                                event.as_object_mut().unwrap().remove("taskUid");
                                match serde_json::from_value::<OutputEvent>(event).unwrap() {
                                    OutputEvent::Chunk(chunk) => {
                                        assert_eq!(
                                            chunk.generation,
                                            created.metadata().generation()
                                        );
                                        assert_eq!(chunk.attempt, 1);
                                        assert!(!chunk.truncated);
                                        assert_eq!(chunk.line.len(), chunk_bytes);
                                        assert!(chunk.line.iter().all(|byte| *byte == b'x'));
                                        let index = match chunk.stream {
                                            StreamKind::Stdout => 0,
                                            StreamKind::Stderr => 1,
                                        };
                                        assert_eq!(chunk.seq, sequences[index]);
                                        sequences[index] += 1;
                                        received += 1;
                                    }
                                    OutputEvent::Lagged { .. } => {
                                        panic!("loss in lossless-size SSE fixture")
                                    }
                                    _ => {}
                                }
                            }
                            total += start.elapsed();
                            drop(stream);
                            assert_eq!(
                                wait_terminal(&fixture.supervisor, created.name())
                                    .await
                                    .phase(),
                                &TaskPhase::Succeeded
                            );
                            fixture.delete(created.name()).await;
                        }
                        fixture.close().await;
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
    writes,
    pagination,
    initial_and_replay,
    live_watch,
    output
);

fn main() {
    benchmark_main("http", benches);
}
