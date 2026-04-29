//! Reference Solti agent - HTTP/JSON transport.
//!
//! A minimal task-execution agent: accepts `TaskSpec` submissions over `/api/v1/tasks`,
//! runs them as subprocesses, reports status back via the same REST surface.
//!
//! Optionally heartbeats to a [Podium](https://github.com/soltiHQ/podium) control-plane so the CP can push specs and collect state remotely.
//!
//! ```bash
//! cargo run -p agentd-http
//! ```
//!
//! Defaults:
//! - API       — `http://localhost:8085/api/v1/tasks`
//! - Metrics   — `http://localhost:9090/metrics`
//! - Heartbeat — `http://localhost:8082` (Podium HTTP discovery)
//! - Override the CP endpoint via `CONTROL_PLANE=http://host:port`.
//!
//! Running without a reachable Podium is fine:
//! the heartbeat task retries with backoff, the local API keeps accepting submissions.
//!
//! ## Observability
//!
//! A dedicated HTTP server runs **under supervisor control** (embedded task with
//! restart-on-failure + backoff) and exposes Prometheus metrics at `/metrics` on
//! port `9090`.
//!
//! ```bash
//! curl http://localhost:9090/metrics
//! ```
//!
//! Metric families:
//! - `solti_runner_*` — runner-level (tasks started/completed, duration histogram, errors).
//! - `solti_sv_*`     — supervision-level (in-flight gauge, restarts, backoff, terminal states, timeouts).
//! - `solti_ctrl_*`   — controller-level (submissions, rejections).
//!
//! See [`solti-prometheus`](../../../crates/solti-prometheus) for the full metric list.
//!
//! ## Talking to the agent
//!
//! ```bash
//! # Submit a one-shot subprocess task
//! curl -X POST http://localhost:8085/api/v1/tasks \
//!   -H "Content-Type: application/json" \
//!   -d '{"spec": {"slot":"hello","kind":{"subprocess":{"mode":{"command":{"command":"echo","args":["hi"]}}}},"timeout":5000,"restart":"never","backoff":{"jitter":"equal","firstMs":1000,"maxMs":5000,"factor":2.0},"admission":"dropIfRunning"}}'
//!
//! # List tasks / get one / list runs / delete
//! curl http://localhost:8085/api/v1/tasks
//! curl http://localhost:8085/api/v1/tasks/{id}
//! curl http://localhost:8085/api/v1/tasks/{id}/runs
//! curl -X DELETE http://localhost:8085/api/v1/tasks/{id}
//! ```
//!
//! Full endpoint reference: [`api_v1.md`](../../../crates/solti-api/api_v1.md).
//! Error envelope, size limits, status codes — all described there.
//!
//! ## Task flow
//!
//! ```text
//!   Client ── submit TaskSpec ─▶ API
//!                                 │
//!                                 ▼
//!                             Supervisor ── owns lifecycle, restart / backoff
//!                                 │
//!                                 │ dispatch by kind + label selectors
//!                                 ▼
//!                             RunnerRouter
//!                                 │
//!                                 ▼
//!                             Runner  (subprocess, wasm, …)
//!                                 │
//!                                 │ lifecycle events
//!                                 ▼
//!                             Subscribers (logs, metrics)
//!
//!   Client ◀── status / runs ── API
//! ```

use std::sync::Arc;

use tracing::info;

use solti_api::{API_VERSION, HttpApi, SupervisorApiAdapter, http_metrics_middleware};
use solti_core::{StateConfig, SupervisorApi};
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{AgentId, RunnerEnv};
use solti_observe::{
    LoggerConfig, LoggerLevel, LoggerTimeZone, TracingEventSubscriber, init_local_offset,
    init_logger, timezone_sync,
};
use solti_prometheus::{
    PrometheusApiMetrics, PrometheusDiscoverMetrics, PrometheusMetrics, PrometheusStateCollector,
    PrometheusSubscriber, Registry, register_build_info, register_process_collector,
    server as metrics_server,
};
use solti_runner::{BuildContext, RunnerRouter};
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

const ADDR: &str = "0.0.0.0:8085";
const METRICS_ADDR: &str = "0.0.0.0:9090";
const CONTROL_PLANE_DEFAULT: &str = "http://localhost:8082";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_local_offset();
    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    init_logger(&LoggerConfig {
        level: LoggerLevel::new("info")?,
        tz: LoggerTimeZone::Local,
        ..Default::default()
    })?;

    let registry = Arc::new(Registry::new());
    let metrics = PrometheusMetrics::new(registry.clone())?;
    let prom_subscriber = PrometheusSubscriber::new(registry.clone())?;
    register_process_collector(&registry)?;
    register_build_info(
        &registry,
        &[
            ("agent", "agentd-http"),
            ("version", env!("CARGO_PKG_VERSION")),
        ],
    )?;

    // Runner: executes TaskSpec bodies (subprocess here).
    let ctx = BuildContext::new(RunnerEnv::default(), Arc::new(metrics));
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default")?;

    // Supervisor: owns every task, applies restart / backoff, fans lifecycle events to subscribers.
    let subscribers: Vec<Arc<dyn Subscribe>> =
        vec![Arc::new(TracingEventSubscriber), Arc::new(prom_subscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
        StateConfig::default(),
    )
    .await?;

    // Pull-based collector: snapshots supervisor state on each /metrics scrape.
    let state_collector = PrometheusStateCollector::new(supervisor.state())?;
    registry.register(Box::new(state_collector))?;

    // Internal tasks travel the same pipeline as user TaskSpecs: submit → dispatch → run.
    let (tz_task, tz_spec) = timezone_sync();
    supervisor.submit_with_task(tz_task, &tz_spec).await?;

    let (m_task, m_spec) = metrics_server(registry.clone(), METRICS_ADDR);
    supervisor.submit_with_task(m_task, &m_spec).await?;

    let control_plane =
        std::env::var("CONTROL_PLANE").unwrap_or_else(|_| CONTROL_PLANE_DEFAULT.to_string());
    let discover_metrics = Arc::new(PrometheusDiscoverMetrics::new(registry.clone())?);
    let discover_config = DiscoverConfig::builder(
        AgentId::new("agentd-http-001"),
        "agentd-http",
        format!("http://{ADDR}"),
        &control_plane,
        DiscoveryTransport::Http,
        10_000,
        API_VERSION,
    )
    .with_metrics(discover_metrics)
    .build()?;
    let (sync_task, sync_spec) = solti_discover::sync(discover_config)?;
    supervisor.submit_with_task(sync_task, &sync_spec).await?;

    // API: client entry point. External TaskSpecs arrive here and flow into the supervisor.
    let api_metrics: Arc<dyn solti_api::ApiMetricsBackend> =
        Arc::new(PrometheusApiMetrics::new(registry.clone())?);
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let app = HttpApi::new(handler)
        .router()
        .layer(axum::middleware::from_fn_with_state(
            api_metrics,
            http_metrics_middleware,
        ));

    let listener = tokio::net::TcpListener::bind(ADDR).await?;
    info!("HTTP http://{ADDR}  →  heartbeat {control_plane}");
    axum::serve(listener, app).await?;
    Ok(())
}
