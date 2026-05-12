//! Reference Solti agent: gRPC transport.
//!
//! A minimal task-execution agent:
//! - Accepts `TaskSpec` submissions via the `solti.v1.SoltiApi` gRPC service
//! - Runs them as subprocesses, reports status back through the same service.
//!
//! Optionally heartbeats to a [Podium](https://github.com/soltiHQ/podium) control-plane so the CP can push specs and collect state remotely.
//! Running without a reachable Podium is fine: the heartbeat task retries with backoff, the local API keeps accepting submissions.
//!
//! ```bash
//! cargo run -p agentd-grpc
//! ```
//!
//! Defaults:
//! - API       - `[::]:50052` (`solti.v1.SoltiApi`)
//! - Metrics   - `http://localhost:9090/metrics` (HTTP, Prometheus scrape target)
//! - Heartbeat - `localhost:50051` (Podium grpc-discovery)
//! - Override the CP endpoint via `CONTROL_PLANE=host:port`.
//!
//! ## Talking to the agent
//!
//! ```bash
//! # List tasks
//! grpcurl -plaintext localhost:50052 solti.v1.SoltiApi/ListTasks
//!
//! # Submit / get / list runs / delete
//! grpcurl -plaintext -d '{"spec": {...}}' localhost:50052 solti.v1.SoltiApi/SubmitTask
//! grpcurl -plaintext -d '{"taskId":"<id>"}' localhost:50052 solti.v1.SoltiApi/GetTaskStatus
//! grpcurl -plaintext -d '{"taskId":"<id>"}' localhost:50052 solti.v1.SoltiApi/ListTaskRuns
//! grpcurl -plaintext -d '{"taskId":"<id>"}' localhost:50052 solti.v1.SoltiApi/DeleteTask
//!
//! # Live-tail stdout/stderr (server-streaming RPC).
//! # One subscription covers all retries of the task with run boundary markers.
//! grpcurl -plaintext -d '{"taskId":"<id>"}' localhost:50052 solti.v1.SoltiApi/StreamTaskLogs
//! ```
//!
//! Proto contract + full RPC surface: [`api_v1.md`](../../../crates/solti-api/api_v1.md).
//! See [`solti-prometheus`](../../../crates/solti-prometheus) for the full metric list.
//!
//! ## Task flow
//!
//! ```text
//!   Client ── submit TaskSpec ─▶ API
//!                                 │
//!                                 ▼
//!                             Supervisor ── owns lifecycle, restart / backoff
//!                                 │ dispatch by kind + label selectors
//!                                 ▼
//!                             RunnerRouter
//!                                 │
//!                                 ▼
//!                             Runner  (subprocess, wasm, …)
//!                                 │ lifecycle events
//!                                 ▼
//!                             Subscribers (logs, metrics)
//!
//!   Client ◀── status / runs ── API
//! ```

use std::sync::Arc;

use tonic::transport::Server;
use tracing::info;

use solti_api::{API_VERSION, SupervisorApiAdapter, build_grpc_server_with_metrics};
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
use solti_runner::{BuildContext, OutputRegistry, RunnerRouter};
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

const ADDR: &str = "[::]:50052";
const ADVERTISED: &str = "localhost:50052";
const METRICS_ADDR: &str = "0.0.0.0:9090";
const CONTROL_PLANE_DEFAULT: &str = "http://localhost:50051";

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
    let subscriber = PrometheusSubscriber::new(registry.clone())?;
    register_process_collector(&registry)?;
    register_build_info(
        &registry,
        &[
            ("agent", "agentd-grpc"),
            ("version", env!("CARGO_PKG_VERSION")),
        ],
    )?;

    // Output registry: live-tail broadcast channels per task.
    let output_registry = Arc::new(OutputRegistry::default());

    // Runner: executes TaskSpec bodies (subprocess here).
    let ctx = BuildContext::new(RunnerEnv::default(), Arc::new(metrics))
        .with_output_registry(Arc::clone(&output_registry));
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default")?;

    // Supervisor: owns every task, applies restart / backoff, fans lifecycle events to subscribers.
    let subscribers: Vec<Arc<dyn Subscribe>> =
        vec![Arc::new(TracingEventSubscriber), Arc::new(subscriber)];
    let supervisor = SupervisorApi::new_with_output_registry(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
        StateConfig::default(),
        Arc::clone(&output_registry),
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
        AgentId::new("agentd-grpc-001"),
        "agentd-grpc",
        ADVERTISED,
        &control_plane,
        DiscoveryTransport::Grpc,
        10_000,
        API_VERSION,
    )
    .with_metrics(discover_metrics)
    .build()?;
    let (sync_task, sync_spec) = solti_discover::sync(discover_config)?;
    supervisor.submit_with_task(sync_task, &sync_spec).await?;

    // API client entry point. External TaskSpecs arrive here and flow into the supervisor.
    let api_metrics: Arc<dyn solti_api::ApiMetricsBackend> =
        Arc::new(PrometheusApiMetrics::new(registry.clone())?);
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let service = build_grpc_server_with_metrics(handler, api_metrics);

    info!("gRPC {ADDR}  →  heartbeat {control_plane}");
    Server::builder()
        .add_service(service)
        .serve(ADDR.parse()?)
        .await?;
    Ok(())
}
