//! # agentd-grpc
//!
//! Reference Solti agent with the gRPC transport. It accepts `TaskSpec`
//! submissions via `solti.task.v1.TaskService` and runs them as subprocesses.
//! Running without a reachable control plane (CP) is fine — the heartbeat
//! retries with backoff while the local API keeps serving.
//!
//! ## What this shows
//!
//! - Accepts `TaskSpec` submissions via `solti.task.v1.TaskService` and reports
//!   status back over the same service.
//! - Runs submitted tasks as subprocesses.
//! - Exposes Prometheus metrics on `:9090` (HTTP).
//! - Heartbeats to a [Podium](https://github.com/soltiHQ/podium) control plane.
//! - Toggles auth and TLS independently via environment variables.
//!
//! ## Run
//!
//! Authentication and TLS are **independent**, each selected by environment
//! variables — giving the four combinations:
//!
//! ```bash
//! # 1) no auth, no TLS (plain dev)
//! cargo run -p agentd-grpc
//!
//! # 2) auth only — bearer token, both directions
//! SOLTI_AGENT_TOKEN=s3cret cargo run -p agentd-grpc
//!
//! # 3) TLS only — serve the API over TLS, dial the CP over TLS
//! SOLTI_TLS_CERT=server.crt SOLTI_TLS_KEY=server.key SOLTI_CP_CA=ca.crt \
//!   cargo run -p agentd-grpc
//!
//! # 4) auth + TLS
//! SOLTI_AGENT_TOKEN=s3cret SOLTI_TLS_CERT=server.crt SOLTI_TLS_KEY=server.key \
//!   SOLTI_CP_CA=ca.crt cargo run -p agentd-grpc
//! ```
//!
//! Environment:
//! - `SOLTI_AGENT_TOKEN` — bearer token; presented to the CP in discovery and
//!   required on this agent's API. Unset → no authentication.
//! - `SOLTI_TLS_CERT` + `SOLTI_TLS_KEY` — serve the API over TLS.
//!   `SOLTI_TLS_CLIENT_CA` (optional) requires client certificates (mTLS).
//! - `SOLTI_CP_CA` — CA that signs the control plane's cert (dial the CP over TLS).
//!   `SOLTI_CP_CLIENT_CERT` + `SOLTI_CP_CLIENT_KEY` (optional) present a client cert (mTLS).
//! - `CONTROL_PLANE` — CP discovery endpoint (default `http://localhost:50051`).
//!
//! ## Next
//!
//! | Example                          | What it adds                                        |
//! |----------------------------------|-----------------------------------------------------|
//! | [`agentd-http`](../../agentd-http) | The same agent over HTTP/JSON instead of gRPC      |
//! | [`podium`](../../podium)         | Config-driven agent: runtime transport switch, TOML |
//! | [`tls-roundtrip`](../../tls-roundtrip) | Minimal mTLS demo of `solti-tls` alone         |

use std::sync::Arc;

use tonic::transport::Server;
use tracing::info;

use solti::api::{
    API_VERSION, ApiMetricsBackend, GrpcApi, SupervisorApiAdapter, to_tonic_server_tls,
};
use solti::core::{StateConfig, SupervisorApi};
use solti::discover::{
    self, AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
    DiscoveryTransport, MonotonicUptime,
};
use solti::exec::subprocess::register_subprocess_runner;
use solti::model::{AgentId, Token};
use solti::observe::{LoggerConfig, LoggerLevel, LoggerTimeZone, init_logger, timezone_sync};
use solti::prometheus::{
    PrometheusApiMetrics, PrometheusCoreStateCollector, PrometheusDiscoverMetrics,
    PrometheusRunnerMetrics, PrometheusTaskvisorSubscriber, Registry, register_build_info,
    register_process_collector, server as metrics_server,
};
use solti::runner::{BuildContext, RunnerEnv, RunnerRouter};
use solti::taskvisor::{ControllerConfig, Subscribe, SupervisorConfig, TracingBridge};
use solti::tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};

const ADDR: &str = "[::]:50052";
const ADVERTISED: &str = "localhost:50052";
const METRICS_ADDR: &str = "0.0.0.0:9090";
const CONTROL_PLANE_DEFAULT: &str = "http://localhost:50051";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    let uptime = Arc::new(MonotonicUptime::new());

    init_logger(&LoggerConfig {
        level: LoggerLevel::new("info")?,
        timezone: LoggerTimeZone::Local,
        ..Default::default()
    })?;

    let registry = Arc::new(Registry::new());
    let metrics = PrometheusRunnerMetrics::new(&registry)?;
    let subscriber = PrometheusTaskvisorSubscriber::new(&registry)?;
    register_process_collector(&registry)?;
    register_build_info(
        &registry,
        &[
            ("agent", "agentd-grpc"),
            ("version", env!("CARGO_PKG_VERSION")),
        ],
    )?;

    // Runner: executes TaskSpec bodies (subprocess here).
    let ctx = BuildContext::new(RunnerEnv::default(), Arc::new(metrics));
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default")?;
    let agent_capabilities = router.capabilities();

    // Supervisor: owns every task, applies restart / backoff, fans events to subscribers.
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingBridge), Arc::new(subscriber)];
    let supervisor = SupervisorApi::builder(router)
        .with_runtime_config(SupervisorConfig::default())
        .with_controller_config(ControllerConfig::default())
        .with_subscribers(subscribers)
        .with_state_config(StateConfig::default())
        .start()
        .await?;

    let state_collector = PrometheusCoreStateCollector::new(supervisor.state())?;
    registry.register(Box::new(state_collector))?;

    // Embedded resources use a prebuilt runtime task and the same supervised lifecycle.
    let (tz_task, tz_task_ref) = timezone_sync();
    supervisor
        .create_embedded_task(tz_task, tz_task_ref)
        .await?;

    let (metrics_task, metrics_task_ref) = metrics_server(
        registry.clone(),
        METRICS_ADDR,
        concat!(env!("CARGO_PKG_NAME"), "@", env!("CARGO_PKG_VERSION")),
    )?;
    supervisor
        .create_embedded_task(metrics_task, metrics_task_ref)
        .await?;

    // --- Optional auth & TLS, selected by the environment ---
    let token = std::env::var("SOLTI_AGENT_TOKEN")
        .ok()
        .map(Token::new)
        .transpose()?;
    let server_tls = server_tls_from_env()?; // agent API server TLS (CP → agent)
    let client_tls = client_tls_from_env()?; // discovery client TLS (agent → CP)

    // --- Discovery: agent → control plane ---
    let control_plane =
        std::env::var("CONTROL_PLANE").unwrap_or_else(|_| CONTROL_PLANE_DEFAULT.to_string());
    let discover_metrics = Arc::new(PrometheusDiscoverMetrics::new(&registry)?);
    let mut discover = DiscoverConfig::builder(
        AgentId::new("agentd-grpc-001")?,
        "agentd-grpc",
        AgentEndpoint::new(ADVERTISED, AgentEndpointType::Grpc, API_VERSION)?,
        ControlPlaneEndpoint::new(&control_plane, DiscoveryTransport::Grpc)?,
        10_000,
        concat!(env!("CARGO_PKG_NAME"), "@", env!("CARGO_PKG_VERSION")),
    )
    .capabilities(agent_capabilities)
    .with_metrics(discover_metrics);
    if let Some(t) = &token {
        discover = discover.with_token(t.clone());
    }
    if let Some(tls) = client_tls {
        discover = discover.with_tls(tls);
    }
    let (sync_task, sync_task_ref) = discover::sync(discover.build()?, uptime)?;
    supervisor
        .create_embedded_task(sync_task, sync_task_ref)
        .await?;

    // --- API: control plane → agent ---
    let api_metrics: Arc<dyn ApiMetricsBackend> = Arc::new(PrometheusApiMetrics::new(&registry)?);
    // Keep the full SDK supervisor so shutdown also drains completion workers.
    let supervisor = Arc::new(supervisor);
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));

    let mut builder = Server::builder();
    let tls_on = server_tls.is_some();
    if let Some(tls) = server_tls {
        builder = builder.tls_config(to_tonic_server_tls(tls)?)?;
    }

    let addr = ADDR.parse()?;
    let scheme = if tls_on { "grpcs" } else { "grpc" };
    info!("{scheme} {ADDR}  →  heartbeat {control_plane}");

    let mut api = GrpcApi::new(handler).with_metrics(api_metrics);
    if let Some(t) = token {
        api = api.with_auth(t);
    }
    builder
        .add_service(api.server())
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    // Drain the supervised runtime: cancels tasks cooperatively (grace period),
    // then force-aborts stragglers. Without this, SIGINT/SIGTERM would kill
    // the process and orphan task subprocesses.
    info!("server stopped; shutting down supervisor");
    supervisor.shutdown().await?;
    Ok(())
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM: the trigger for graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Reads a non-empty environment variable as a path.
fn env_path(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// Agent API server TLS (the control plane connects to this).
///
/// Enabled when `SOLTI_TLS_CERT` and `SOLTI_TLS_KEY` are both set;
/// `SOLTI_TLS_CLIENT_CA` (optional) turns on mTLS (require a client cert).
fn server_tls_from_env() -> Result<Option<ServerTlsConfig>, Box<dyn std::error::Error>> {
    let identity = match (env_path("SOLTI_TLS_CERT"), env_path("SOLTI_TLS_KEY")) {
        (None, None) => return Ok(None),
        (Some(cert), Some(key)) => TlsIdentity::from_pem_files(cert, key),
        _ => return Err("SOLTI_TLS_CERT and SOLTI_TLS_KEY must be set together".into()),
    };
    let mut tls = ServerTlsConfig::new(identity);
    if let Some(ca) = env_path("SOLTI_TLS_CLIENT_CA") {
        tls = tls.require_client_auth(TrustRoots::from_pem_file(ca));
    }
    Ok(Some(tls))
}

/// Discovery client TLS (the agent dials the control plane over TLS).
///
/// Enabled when `SOLTI_CP_CA` is set; `SOLTI_CP_CLIENT_CERT` + `SOLTI_CP_CLIENT_KEY`
/// (optional) present a client cert for mTLS.
fn client_tls_from_env() -> Result<Option<ClientTlsConfig>, Box<dyn std::error::Error>> {
    let Some(ca) = env_path("SOLTI_CP_CA") else {
        return Ok(None);
    };
    let mut tls = ClientTlsConfig::new(TrustRoots::from_pem_file(ca));
    match (
        env_path("SOLTI_CP_CLIENT_CERT"),
        env_path("SOLTI_CP_CLIENT_KEY"),
    ) {
        (None, None) => {}
        (Some(cert), Some(key)) => {
            tls = tls.with_identity(TlsIdentity::from_pem_files(cert, key));
        }
        _ => {
            return Err("SOLTI_CP_CLIENT_CERT and SOLTI_CP_CLIENT_KEY must be set together".into());
        }
    }
    Ok(Some(tls))
}
