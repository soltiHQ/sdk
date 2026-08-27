//! Task execution with real SDK metrics/logging and a controlled local metrics endpoint.

use std::num::NonZeroUsize;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use prometheus::{
    Gauge, GaugeVec, Opts,
    core::{Collector, Desc},
    proto::MetricFamily,
};
use solti_benches::fixtures::{RUNTIMES, WAIT_BOUND, bounded, embedded_manifest, wait_terminal};
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};
use solti_core::SupervisorApi;
use solti_model::{TaskManifest, TaskPhase};
use solti_observe::{LoggerConfig, LoggerFormat, LoggerLevel, init_logger};
use solti_prometheus::{
    MetricsServerConfig, PrometheusCoreStateCollector, PrometheusTaskvisorSubscriber, Registry,
    server_with_config,
};
use solti_runner::RunnerRouter;
use taskvisor::{Subscribe, TaskContext, TaskFn, TaskRef};

#[path = "boundary_support/observability.rs"]
mod observability_support;

use observability_support::{scrape, setup_scrape, wait_for_collector_entry};

const BATCH: usize = 32;
const LOG_RECORDS: usize = 8;
const TASKS: CaseFamily = CaseFamily::lifecycle(
    "observability/steady/task_batch",
    "OBSERVABILITY · FIXED TASK BATCH",
    "observed task",
    "observed tasks",
    "commit 32 Queue embedded tasks through observed SDK Succeeded states; scrape variant also completes one concurrent HTTP scrape",
    "runtime/core/registry/subscriber/server setup, manifest/task allocation, one warm-up batch, metrics drain and task deletion",
).without_lifecycle_interpretation();
const SCRAPE: CaseFamily = CaseFamily::query(
    "observability/steady/scrape",
    "METRICS · COMPLETE HTTP SCRAPE",
    "scrape",
    "scrapes",
    "issue local HTTP requests through complete exposition reads and expected-series validation",
    "registry population, runtime/core/metrics-server setup, first scrape and shutdown",
);
const SATURATED: CaseFamily = CaseFamily::policy(
    "observability/steady/saturated_scrape",
    "METRICS · SATURATED SCRAPE REJECTION",
    "rejected scrape",
    "rejected scrapes",
    "HTTP request through complete 503 response while one controlled physical collector owns the only slot",
    "server setup, admission retries while opening the held collector, releasing it and verifying recovery",
);
const LOGGING: CaseFamily = CaseFamily::lifecycle(
    "observability/steady/logged_task_batch",
    "LOGGING · SAME TASK BATCH THROUGH SDK LOGGER",
    "observed task",
    "observed tasks",
    "commit 32 Queue tasks with eight fixed tracing records each through observed SDK Succeeded states",
    "isolated child process/runtime/core/logger setup, task allocation, one warm-up batch, task deletion and shutdown; stdout sink is null",
).without_lifecycle_interpretation();

fn batch_fixture() -> (Vec<TaskManifest>, Vec<TaskRef>) {
    let manifests = (0..BATCH)
        .map(|index| {
            let name = format!("observed-{index}");
            embedded_manifest(&name, &name)
        })
        .collect();
    let tasks = (0..BATCH)
        .map(|index| -> TaskRef {
            TaskFn::arc(move |_: TaskContext| async move {
                for step in 0..LOG_RECORDS {
                    tracing::info!(
                        target: "solti_benches::workload",
                        event = "bench.progress", task_index = index, step,
                        payload = "fixed-benchmark-record", "controlled task progress"
                    );
                }
                Ok(())
            })
        })
        .collect();
    (manifests, tasks)
}

async fn submit_batch(supervisor: &SupervisorApi, manifests: &[TaskManifest], tasks: &[TaskRef]) {
    for (manifest, task) in manifests.iter().zip(tasks) {
        bounded(supervisor.create_embedded_task(manifest.clone(), Arc::clone(task)))
            .await
            .unwrap();
    }
    for manifest in manifests {
        assert_eq!(
            wait_terminal(supervisor, manifest.name()).await.phase(),
            &TaskPhase::Succeeded
        );
    }
}

async fn delete_batch(supervisor: &SupervisorApi, manifests: &[TaskManifest]) {
    for manifest in manifests {
        bounded(supervisor.delete_task(manifest.name()))
            .await
            .unwrap();
    }
}

struct Fixture {
    supervisor: Arc<SupervisorApi>,
    registry: Arc<Registry>,
    client: reqwest::Client,
    url: Option<String>,
}

impl Fixture {
    async fn new(
        metrics: bool,
        http: bool,
        registry: Arc<Registry>,
        config: MetricsServerConfig,
    ) -> Self {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let subscribers: Vec<Arc<dyn Subscribe>> = if metrics {
            vec![Arc::new(
                PrometheusTaskvisorSubscriber::with_queue_capacity(
                    &registry,
                    NonZeroUsize::new(4_096).unwrap(),
                )
                .unwrap(),
            )]
        } else {
            Vec::new()
        };
        let supervisor = Arc::new(
            bounded(
                SupervisorApi::builder(RunnerRouter::new())
                    .with_subscribers(subscribers)
                    .start(),
            )
            .await
            .unwrap(),
        );
        if metrics {
            registry
                .register(Box::new(
                    PrometheusCoreStateCollector::new(supervisor.state()).unwrap(),
                ))
                .unwrap();
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(WAIT_BOUND)
            .build()
            .unwrap();
        let url = if http {
            let marker = Gauge::new(
                "solti_bench_local_fixture",
                "Confirms the benchmark endpoint",
            )
            .unwrap();
            marker.set(1.0);
            registry.register(Box::new(marker)).unwrap();
            // The public server API accepts an address, not an already-bound listener.
            // Reserve an ephemeral port immediately before asking the SDK task to bind it.
            let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = reservation.local_addr().unwrap();
            let (manifest, task) = server_with_config(
                Arc::clone(&registry),
                address.to_string(),
                "bench-v1",
                config,
            )
            .unwrap();
            drop(reservation);
            bounded(supervisor.create_embedded_task(manifest, task))
                .await
                .unwrap();
            let url = format!("http://{address}/metrics");
            bounded(async {
                loop {
                    if let Ok(response) = client.get(&url).send().await
                        && response.status().is_success()
                        && response
                            .text()
                            .await
                            .unwrap()
                            .contains("solti_bench_local_fixture 1")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await;
            Some(url)
        } else {
            None
        };
        Self {
            supervisor,
            registry,
            client,
            url,
        }
    }

    async fn close(self) {
        drop(self.client);
        bounded(self.supervisor.shutdown()).await.unwrap();
    }

    async fn drain_metrics(&self, expected: u64) {
        bounded(async {
            loop {
                let families = self.registry.gather();
                let mut completed = 0.0;
                for family in families {
                    if family.name() == "solti_taskvisor_task_final_outcomes_total" {
                        completed = family
                            .get_metric()
                            .iter()
                            .map(|metric| metric.get_counter().value())
                            .sum();
                    }
                    if family.name() == "solti_taskvisor_subscriber_overflows_total" {
                        assert!(
                            family
                                .get_metric()
                                .iter()
                                .all(|metric| metric.get_counter().value() == 0.0),
                            "metrics subscriber lost events"
                        );
                    }
                }
                if completed >= expected as f64 {
                    assert_eq!(
                        completed, expected as f64,
                        "unexpected extra terminal metric"
                    );
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }
}

fn task_metrics(c: &mut Criterion) {
    print_suite_header("observability");
    let mut group = c.benchmark_group(TASKS.group_id);
    group.throughput(Throughput::Elements(BATCH as u64));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for (mode, metrics, http) in [
            ("noop", false, false),
            ("metrics", true, false),
            ("metrics_and_scrape", true, true),
        ] {
            group.bench_function(BenchmarkId::new(runtime_name, mode), |b| {
                record_case(TASKS, runtime_name, Some(mode.into()));
                let runtime = make_runtime();
                let (manifests, tasks) = batch_fixture();
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let fixture = Fixture::new(
                            metrics,
                            http,
                            Arc::new(Registry::new()),
                            MetricsServerConfig::default(),
                        )
                        .await;
                        submit_batch(&fixture.supervisor, &manifests, &tasks).await;
                        if metrics {
                            fixture.drain_metrics(BATCH as u64).await;
                        }
                        delete_batch(&fixture.supervisor, &manifests).await;
                        let mut total = Duration::ZERO;
                        for iteration in 0..iterations {
                            let start = Instant::now();
                            let pending_scrape = fixture.url.as_ref().map(|url| {
                                tokio::spawn(scrape(fixture.client.clone(), url.clone(), None))
                            });
                            submit_batch(&fixture.supervisor, &manifests, &tasks).await;
                            if let Some(pending) = pending_scrape {
                                bounded(pending).await.unwrap();
                            }
                            total += start.elapsed();
                            if metrics {
                                fixture.drain_metrics((iteration + 2) * BATCH as u64).await;
                            }
                            delete_batch(&fixture.supervisor, &manifests).await;
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

fn scrapes(c: &mut Criterion) {
    let mut group = c.benchmark_group(SCRAPE.group_id);
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for series in [128, 1_024] {
            for clients in [1, 4] {
                let variant = format!("{series}_series/{clients}_clients");
                group.throughput(Throughput::Elements(clients));
                group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                    record_case(SCRAPE, runtime_name, Some(variant.clone()));
                    let runtime = make_runtime();
                    b.iter_custom(|iterations| {
                        runtime.block_on(async {
                            let registry = Arc::new(Registry::new());
                            let metrics = GaugeVec::new(
                                Opts::new(
                                    "solti_bench_series",
                                    "Controlled exposition cardinality",
                                ),
                                &["item"],
                            )
                            .unwrap();
                            for index in 0..series {
                                metrics
                                    .with_label_values(&[&index.to_string()])
                                    .set(index as f64);
                            }
                            registry.register(Box::new(metrics)).unwrap();
                            let config = MetricsServerConfig::new()
                                .try_with_max_concurrent_scrapes(4)
                                .unwrap();
                            let fixture = Fixture::new(true, true, registry, config).await;
                            let mut total = Duration::ZERO;
                            for _ in 0..iterations {
                                let start = Instant::now();
                                let pending: Vec<_> = (0..clients)
                                    .map(|_| {
                                        tokio::spawn(scrape(
                                            fixture.client.clone(),
                                            fixture.url.clone().unwrap(),
                                            Some(series),
                                        ))
                                    })
                                    .collect();
                                for request in pending {
                                    bounded(request).await.unwrap();
                                }
                                total += start.elapsed();
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

#[derive(Default)]
struct CollectorGate {
    armed: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
    entered: tokio::sync::Notify,
}

impl CollectorGate {
    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

struct ReleaseOnDrop(Arc<CollectorGate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct ControlledCollector {
    gauge: Gauge,
    gate: Arc<CollectorGate>,
}

impl Collector for ControlledCollector {
    fn desc(&self) -> Vec<&Desc> {
        self.gauge.desc()
    }
    fn collect(&self) -> Vec<MetricFamily> {
        if self.gate.armed.swap(false, Ordering::AcqRel) {
            self.gate.entered.notify_one();
            let released = self
                .gate
                .released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (released, wait) = self
                .gate
                .changed
                .wait_timeout_while(released, WAIT_BOUND, |released| !*released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(released);
            assert!(
                !wait.timed_out(),
                "fixture did not release its physical collector"
            );
        }
        self.gauge.collect()
    }
}

fn saturation(c: &mut Criterion) {
    let mut group = c.benchmark_group(SATURATED.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        group.bench_function(runtime_name, |b| {
            record_case(SATURATED, runtime_name, None);
            let runtime = make_runtime();
            b.iter_custom(|iterations| {
                runtime.block_on(async {
                    let registry = Arc::new(Registry::new());
                    let gate = Arc::new(CollectorGate::default());
                    registry
                        .register(Box::new(ControlledCollector {
                            gauge: Gauge::new(
                                "solti_bench_held_collector",
                                "Controlled physical gather",
                            )
                            .unwrap(),
                            gate: Arc::clone(&gate),
                        }))
                        .unwrap();
                    let config = MetricsServerConfig::new()
                        .try_with_max_concurrent_scrapes(1)
                        .unwrap();
                    let fixture = Fixture::new(false, true, registry, config).await;
                    let mut total = Duration::ZERO;
                    for _ in 0..iterations {
                        *gate.released.lock().unwrap() = false;
                        gate.armed.store(true, Ordering::Release);
                        let release_guard = ReleaseOnDrop(Arc::clone(&gate));
                        let mut first = tokio::spawn(setup_scrape(
                            fixture.client.clone(),
                            fixture.url.clone().unwrap(),
                        ));
                        wait_for_collector_entry(&gate.entered, &mut first).await;
                        let start = Instant::now();
                        let response =
                            bounded(fixture.client.get(fixture.url.as_ref().unwrap()).send())
                                .await
                                .unwrap();
                        assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
                        bounded(response.bytes()).await.unwrap();
                        total += start.elapsed();
                        drop(release_guard);
                        bounded(first).await.unwrap();
                        setup_scrape(fixture.client.clone(), fixture.url.clone().unwrap()).await;
                    }
                    fixture.close().await;
                    total
                })
            });
        });
    }
    group.finish();
}

fn logging_worker(args: &[String]) -> bool {
    if args.get(1).map(String::as_str) != Some("--solti-logging-worker") {
        return false;
    }
    let mode = args.get(2).expect("logging mode");
    let runtime_name = args.get(3).expect("logging runtime");
    let iterations: u64 = args.get(4).expect("logging iterations").parse().unwrap();
    let config = LoggerConfig {
        format: if mode == "json" {
            LoggerFormat::Json
        } else {
            LoggerFormat::Text
        },
        level: LoggerLevel::new(if mode == "off" {
            "off"
        } else {
            "off,solti_benches::workload=info"
        })
        .unwrap(),
        use_color: false,
        ..Default::default()
    };
    assert!(matches!(mode.as_str(), "off" | "text" | "json"));
    init_logger(&config).unwrap();
    assert_eq!(
        tracing::enabled!(target: "solti_benches::workload", tracing::Level::INFO),
        mode != "off"
    );
    let runtime = RUNTIMES
        .iter()
        .find(|(name, _)| name == runtime_name)
        .unwrap()
        .1();
    let (manifests, tasks) = batch_fixture();
    let total = runtime.block_on(async {
        let supervisor = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
            .await
            .unwrap();
        submit_batch(&supervisor, &manifests, &tasks).await;
        delete_batch(&supervisor, &manifests).await;
        let mut total = Duration::ZERO;
        for _ in 0..iterations {
            let start = Instant::now();
            submit_batch(&supervisor, &manifests, &tasks).await;
            total += start.elapsed();
            delete_batch(&supervisor, &manifests).await;
        }
        bounded(supervisor.shutdown()).await.unwrap();
        total
    });
    eprintln!(
        "solti-bench-result:{}:{}",
        total.as_nanos(),
        iterations * BATCH as u64
    );
    true
}

fn logging(c: &mut Criterion) {
    let mut group = c.benchmark_group(LOGGING.group_id);
    group.throughput(Throughput::Elements(BATCH as u64));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for mode in ["off", "text", "json"] {
            group.bench_function(BenchmarkId::new(runtime_name, mode), |b| {
                record_case(LOGGING, runtime_name, Some(mode.into()));
                let runtime = make_runtime();
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let output = bounded(
                            tokio::process::Command::new(std::env::current_exe().unwrap())
                                .args([
                                    "--solti-logging-worker",
                                    mode,
                                    runtime_name,
                                    &iterations.to_string(),
                                ])
                                .stdin(Stdio::null())
                                .stdout(Stdio::null())
                                .stderr(Stdio::piped())
                                .kill_on_drop(true)
                                .output(),
                        )
                        .await
                        .unwrap();
                        let stderr = String::from_utf8(output.stderr).unwrap();
                        assert!(output.status.success(), "logging worker failed: {stderr}");
                        let result = stderr
                            .lines()
                            .find_map(|line| line.strip_prefix("solti-bench-result:"))
                            .expect("worker timing result");
                        let (nanos, completed) = result.split_once(':').unwrap();
                        assert_eq!(completed.parse::<u64>().unwrap(), iterations * BATCH as u64);
                        Duration::from_nanos(nanos.parse().unwrap())
                    })
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, task_metrics, scrapes, saturation, logging);

fn main() {
    if !logging_worker(&std::env::args().collect::<Vec<_>>()) {
        benchmark_main("observability", benches);
    }
}
