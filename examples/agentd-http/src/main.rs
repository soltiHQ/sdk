//! # agentd-http
//!
//! Reference Solti agent with the HTTP/JSON transport. It accepts `TaskSpec`
//! submissions over `/api/v1/tasks` and runs them as subprocesses. Running
//! without a reachable control plane (CP) is fine — the heartbeat retries with
//! backoff while the local API keeps serving.
//!
//! ## What this shows
//!
//! - Accepts `TaskSpec` submissions over `/api/v1/tasks` and reports status
//!   back over the same REST surface.
//! - Runs submitted tasks as subprocesses.
//! - Exposes Prometheus metrics on `:9090`.
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
//! cargo run -p agentd-http
//!
//! # 2) auth only — bearer token, both directions
//! SOLTI_AGENT_TOKEN=s3cret cargo run -p agentd-http
//!
//! # 3) TLS only — serve the API over TLS, dial the CP over TLS
//! SOLTI_TLS_CERT=server.crt SOLTI_TLS_KEY=server.key SOLTI_CP_CA=ca.crt \
//!   cargo run -p agentd-http
//!
//! # 4) auth + TLS
//! SOLTI_AGENT_TOKEN=s3cret SOLTI_TLS_CERT=server.crt SOLTI_TLS_KEY=server.key \
//!   SOLTI_CP_CA=ca.crt cargo run -p agentd-http
//! ```
//!
//! Environment:
//! - `SOLTI_AGENT_TOKEN` — bearer token; presented to the CP in discovery and
//!   required on this agent's API. Unset → no authentication.
//! - `SOLTI_TLS_CERT` + `SOLTI_TLS_KEY` — serve the API over TLS.
//!   `SOLTI_TLS_CLIENT_CA` (optional) requires client certificates (mTLS).
//! - `SOLTI_CP_CA` — CA that signs the control plane's cert (dial the CP over TLS).
//!   `SOLTI_CP_CLIENT_CERT` + `SOLTI_CP_CLIENT_KEY` (optional) present a client cert (mTLS).
//! - `CONTROL_PLANE` — CP discovery endpoint (default `http://localhost:8082`).
//!
//! Talking to the agent and the full endpoint reference live in
//! [`api_v1.md`](../../../crates/solti-api/api_v1.md).
//!
//! ## Next
//!
//! | Example                          | What it adds                                        |
//! |----------------------------------|-----------------------------------------------------|
//! | [`agentd-grpc`](../../agentd-grpc) | The same agent over gRPC instead of HTTP/JSON      |
//! | [`podium`](../../podium)         | Config-driven agent: runtime transport switch, TOML |
//! | [`tls-roundtrip`](../../tls-roundtrip) | Minimal mTLS demo of `solti-tls` alone         |

use std::sync::Arc;

use axum_server::tls_rustls::RustlsConfig;
use tracing::info;

use solti_api::{API_VERSION, HttpApi, SupervisorApiAdapter, http_metrics_middleware};
use solti_core::{StateConfig, SupervisorApi};
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{AgentId, RunnerEnv, Token};
use solti_observe::{
    LoggerConfig, LoggerLevel, LoggerTimeZone, TracingBridge, init_local_offset, init_logger,
    timezone_sync,
};
use solti_prometheus::{
    PrometheusApiMetrics, PrometheusDiscoverMetrics, PrometheusMetrics, PrometheusStateCollector,
    PrometheusSubscriber, Registry, register_build_info, register_process_collector,
    server as metrics_server,
};
use solti_runner::{BuildContext, OutputRegistry, RunnerRouter};
use solti_tls::{ClientTlsConfig, ServerTlsConfig};
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

    // Output registry: live-tail broadcast channels per task.
    let output_registry = Arc::new(OutputRegistry::default());

    // Runner: executes TaskSpec bodies (subprocess here).
    let ctx = BuildContext::new(RunnerEnv::default(), Arc::new(metrics))
        .with_output_registry(Arc::clone(&output_registry));
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default")?;

    // Supervisor: owns every task, applies restart / backoff, fans events to subscribers.
    let subscribers: Vec<Arc<dyn Subscribe>> =
        vec![Arc::new(TracingBridge), Arc::new(prom_subscriber)];
    let supervisor = SupervisorApi::new_with_output_registry(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
        StateConfig::default(),
        Arc::clone(&output_registry),
    )
    .await?;

    let state_collector = PrometheusStateCollector::new(supervisor.state())?;
    registry.register(Box::new(state_collector))?;

    // Internal tasks travel the same pipeline as user TaskSpecs: submit → dispatch → run.
    let (tz_task, tz_spec) = timezone_sync();
    supervisor.submit_with_task(tz_task, &tz_spec).await?;

    let (m_task, m_spec) = metrics_server(registry.clone(), METRICS_ADDR);
    supervisor.submit_with_task(m_task, &m_spec).await?;

    // --- Optional auth & TLS, selected by the environment ---
    let token = std::env::var("SOLTI_AGENT_TOKEN").ok().map(Token::new);
    let server_tls = server_tls_from_env()?; // agent API server TLS (CP → agent)
    let client_tls = client_tls_from_env()?; // discovery client TLS (agent → CP)

    // --- Discovery: agent → control plane ---
    let control_plane =
        std::env::var("CONTROL_PLANE").unwrap_or_else(|_| CONTROL_PLANE_DEFAULT.to_string());
    let discover_metrics = Arc::new(PrometheusDiscoverMetrics::new(registry.clone())?);
    let mut discover = DiscoverConfig::builder(
        AgentId::new("agentd-http-001"),
        "agentd-http",
        format!("http://{ADDR}"),
        &control_plane,
        DiscoveryTransport::Http,
        10_000, // heartbeat interval (ms)
        API_VERSION,
    )
    .with_metrics(discover_metrics);
    if let Some(t) = &token {
        discover = discover.with_token(t.clone());
    }
    if let Some(tls) = client_tls {
        discover = discover.with_tls(tls);
    }
    let (sync_task, sync_spec) = solti_discover::sync(discover.build()?)?;
    supervisor.submit_with_task(sync_task, &sync_spec).await?;

    // --- API: control plane → agent ---
    let api_metrics: Arc<dyn solti_api::ApiMetricsBackend> =
        Arc::new(PrometheusApiMetrics::new(registry.clone())?);
    // Keep a supervisor handle for graceful shutdown after the server stops.
    let sup_handle = supervisor.handle();
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));

    let mut api = HttpApi::new(handler);
    if let Some(t) = token {
        api = api.with_auth(t);
    }
    let app = api.router().layer(axum::middleware::from_fn_with_state(
        api_metrics,
        http_metrics_middleware,
    ));

    let scheme = if server_tls.is_some() {
        "https"
    } else {
        "http"
    };
    info!("{scheme} {ADDR}  →  heartbeat {control_plane}");

    match server_tls {
        Some(tls) => {
            let rustls = Arc::new(tls.into_rustls_config()?);
            let srv = axum_server::Handle::new();
            let srv2 = srv.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                srv2.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
            });
            axum_server::bind_rustls(ADDR.parse()?, RustlsConfig::from_config(rustls))
                .handle(srv)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(ADDR).await?;
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }

    // Drain the supervision tree: cancels tasks cooperatively (grace period),
    // then force-aborts stragglers. Without this, SIGINT/SIGTERM would kill
    // the process and orphan task subprocesses.
    info!("server stopped; shutting down supervisor");
    sup_handle.shutdown().await?;
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
    let (Some(cert), Some(key)) = (env_path("SOLTI_TLS_CERT"), env_path("SOLTI_TLS_KEY")) else {
        return Ok(None);
    };
    let mut b = ServerTlsConfig::builder()
        .cert_pem_file(cert)
        .key_pem_file(key);
    if let Some(ca) = env_path("SOLTI_TLS_CLIENT_CA") {
        b = b.require_client_ca_pem_file(ca);
    }
    Ok(Some(b.build()?))
}

/// Discovery client TLS (the agent dials the control plane over TLS).
///
/// Enabled when `SOLTI_CP_CA` is set; `SOLTI_CP_CLIENT_CERT` + `SOLTI_CP_CLIENT_KEY`
/// (optional) present a client cert for mTLS.
fn client_tls_from_env() -> Result<Option<ClientTlsConfig>, Box<dyn std::error::Error>> {
    let Some(ca) = env_path("SOLTI_CP_CA") else {
        return Ok(None);
    };
    let mut b = ClientTlsConfig::builder().ca_pem_file(ca);
    if let (Some(cert), Some(key)) = (
        env_path("SOLTI_CP_CLIENT_CERT"),
        env_path("SOLTI_CP_CLIENT_KEY"),
    ) {
        b = b.client_cert_pem_file(cert).client_key_pem_file(key);
    }
    Ok(Some(b.build()?))
}
