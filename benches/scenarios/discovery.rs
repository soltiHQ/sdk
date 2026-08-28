//! Supervised discovery against a controlled loopback HTTP endpoint.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};
use serde_json::{Value, json};
use solti_benches::fixtures::{RUNTIMES, bounded};
use solti_benches::report::{CaseFamily, benchmark_main, print_suite_header, record_case};
use solti_core::SupervisorApi;
use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig, DiscoverFailReason,
    DiscoverMetricsBackend, DiscoveryTransport, sync,
};
use solti_model::{AgentCapabilities, AgentId, BackoffPolicy, JitterPolicy};
use solti_runner::RunnerRouter;
use tokio::sync::{mpsc, oneshot};

const COLD: CaseFamily = CaseFamily::lifecycle(
    "discovery/cold/first_success",
    "DISCOVERY · FIRST SUPERVISED HEARTBEAT",
    "accepted heartbeat",
    "accepted heartbeats",
    "commit the embedded sync task through its first successful HTTP response and success callback",
    "runtime/supervisor/server/client/config setup and shutdown; startup jitter (delay=1 ms) is inside",
)
.without_lifecycle_interpretation();
const RECOVER: CaseFamily = CaseFamily::lifecycle(
    "discovery/cold/retry_then_success",
    "DISCOVERY · SUPERVISED RECOVERY FROM HTTP 503",
    "recovered heartbeat",
    "recovered heartbeats",
    "commit sync task, receive one HTTP 503, Taskvisor retry/backoff, then successful HTTP response",
    "runtime/supervisor/server/client/config setup and shutdown; configured 1 ms backoff is inside",
)
.without_lifecycle_interpretation();
const WARM: CaseFamily = CaseFamily::query(
    "discovery/steady/request",
    "DISCOVERY · REUSED CLIENT REQUEST",
    "accepted request",
    "accepted requests",
    "high-resolution attempt callback timestamp through HTTP response validation and success callback",
    "first heartbeat, periodic delay, timestamp/uptime stamping, setup and shutdown; callback overhead is inside",
);

#[derive(Debug)]
struct Probe {
    started: Mutex<Option<Instant>>,
    successes: mpsc::UnboundedSender<Duration>,
    attempts: AtomicU64,
    failures: AtomicU64,
}

impl DiscoverMetricsBackend for Probe {
    fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        *self.started.lock().unwrap() = Some(Instant::now());
    }

    fn record_success(&self, _: u64) {
        let started = self
            .started
            .lock()
            .unwrap()
            .take()
            .expect("attempt started");
        let _ = self.successes.send(started.elapsed());
    }

    fn record_failure(&self, _: u64, reason: DiscoverFailReason) {
        assert_eq!(reason, DiscoverFailReason::RejectedServer);
        self.started
            .lock()
            .unwrap()
            .take()
            .expect("attempt started");
        self.failures.fetch_add(1, Ordering::Relaxed);
    }
}

struct Endpoint {
    requests: AtomicU64,
    fail_first: bool,
}

async fn heartbeat(
    State(state): State<Arc<Endpoint>>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    assert!(request.is_object(), "discovery sends a JSON object");
    let request_number = state.requests.fetch_add(1, Ordering::Relaxed);
    if state.fail_first && request_number == 0 {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "controlled retry"})),
        )
    } else {
        (StatusCode::OK, Json(json!({"success": true})))
    }
}

struct Fixture {
    supervisor: SupervisorApi,
    manifest: solti_model::TaskManifest,
    task: taskvisor::TaskRef,
    probe: Arc<Probe>,
    successes: mpsc::UnboundedReceiver<Duration>,
    endpoint: Arc<Endpoint>,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(fail_first: bool, metadata_bytes: usize) -> Self {
        let endpoint = Arc::new(Endpoint {
            requests: AtomicU64::new(0),
            fail_first,
        });
        let app = Router::new()
            .route("/api/v1/discovery/sync", post(heartbeat))
            .with_state(Arc::clone(&endpoint));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopping) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopping.await;
                })
                .await
                .unwrap();
        });
        let (successes_tx, successes) = mpsc::unbounded_channel();
        let probe = Arc::new(Probe {
            started: Mutex::new(None),
            successes: successes_tx,
            attempts: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        });
        let config = DiscoverConfig::builder(
            AgentId::new("benchmark-agent").unwrap(),
            "benchmark-agent",
            AgentEndpoint::new("http://127.0.0.1:1", AgentEndpointType::Http, 1).unwrap(),
            ControlPlaneEndpoint::new(format!("http://{address}"), DiscoveryTransport::Http)
                .unwrap(),
            AgentCapabilities::new(Vec::new()).unwrap(),
            1,
            "bench-v1",
        )
        .metadata(HashMap::from([(
            "payload".into(),
            "x".repeat(metadata_bytes),
        )]))
        .connect_timeout_ms(5_000)
        .request_timeout_ms(5_000)
        .backoff(BackoffPolicy {
            first_ms: 1,
            max_ms: 1,
            factor: 1.0,
            jitter: JitterPolicy::None,
        })
        .with_metrics(probe.clone())
        .build()
        .unwrap();
        // This is an advertised address, not a contacted endpoint. All requests go to `address`.
        let (manifest, task) = sync(config, Arc::new(|| 42_u64)).unwrap();
        let supervisor = bounded(SupervisorApi::builder(RunnerRouter::new()).start())
            .await
            .unwrap();
        Self {
            supervisor,
            manifest,
            task,
            probe,
            successes,
            endpoint,
            stop,
            server,
        }
    }

    async fn start(&self) {
        bounded(
            self.supervisor
                .create_embedded_task(self.manifest.clone(), Arc::clone(&self.task)),
        )
        .await
        .unwrap();
    }

    async fn next_success(&mut self) -> Duration {
        bounded(self.successes.recv())
            .await
            .expect("discovery success callback")
    }

    async fn finish(self) {
        bounded(self.supervisor.shutdown()).await.unwrap();
        self.stop.send(()).unwrap();
        bounded(self.server).await.unwrap();
    }
}

fn cold_and_recovery(c: &mut Criterion) {
    print_suite_header("discovery");
    for (family, fail_first) in [(COLD, false), (RECOVER, true)] {
        let mut group = c.benchmark_group(family.group_id);
        group.throughput(Throughput::Elements(1));
        for &(runtime_name, make_runtime) in &RUNTIMES {
            group.bench_function(runtime_name, |b| {
                record_case(family, runtime_name, None);
                let runtime = make_runtime();
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            let mut fixture = Fixture::new(fail_first, 64).await;
                            let start = Instant::now();
                            fixture.start().await;
                            fixture.next_success().await;
                            total += start.elapsed();
                            assert_eq!(
                                fixture.probe.failures.load(Ordering::Relaxed),
                                u64::from(fail_first)
                            );
                            assert!(
                                fixture.endpoint.requests.load(Ordering::Relaxed)
                                    >= if fail_first { 2 } else { 1 }
                            );
                            fixture.finish().await;
                        }
                        total
                    })
                });
            });
        }
        group.finish();
    }
}

fn warm_requests(c: &mut Criterion) {
    let mut group = c.benchmark_group(WARM.group_id);
    group.throughput(Throughput::Elements(1));
    for &(runtime_name, make_runtime) in &RUNTIMES {
        for metadata_bytes in [64, 4_096] {
            let variant = format!("{metadata_bytes}_metadata_bytes");
            group.bench_function(BenchmarkId::new(runtime_name, &variant), |b| {
                record_case(WARM, runtime_name, Some(variant.clone()));
                let runtime = make_runtime();
                b.iter_custom(|iterations| {
                    runtime.block_on(async {
                        let mut fixture = Fixture::new(false, metadata_bytes).await;
                        fixture.start().await;
                        fixture.next_success().await;
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            total += fixture.next_success().await;
                        }
                        assert_eq!(fixture.probe.failures.load(Ordering::Relaxed), 0);
                        assert!(fixture.endpoint.requests.load(Ordering::Relaxed) > iterations);
                        fixture.finish().await;
                        total
                    })
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, cold_and_recovery, warm_requests);

fn main() {
    benchmark_main("discovery", benches);
}
