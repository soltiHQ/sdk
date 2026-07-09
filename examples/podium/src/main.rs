//! # podium
//!
//! Reference Solti agent for a **Podium** control-plane — one binary,
//! config-driven. Running without a reachable control plane is fine — the
//! heartbeat retries with backoff while the local API keeps serving.
//!
//! ## What this shows
//!
//! - Picks its transport (**HTTP** or **gRPC**) at runtime from a TOML config.
//! - Exposes its own `TaskService`; podium can push specs to it.
//! - Registers + heartbeats to podium's discovery endpoint.
//! - Toggles **TLS / mTLS** and a shared **bearer token** on or off.
//! - Runs one local demo task; there is something to see immediately.
//!
//! ## Run
//!
//! ```bash
//! cargo run -p podium -- --config examples/podium/config.toml
//! ```
//!
//! See `README.md` for the full walkthrough (cert generation + the matching
//! podium environment; it lines up the two halves of the connection).
//!
//! ## Next
//!
//! | Example                          | What it adds                                      |
//! |----------------------------------|---------------------------------------------------|
//! | [`agentd-http`](../../agentd-http) | Env-driven agent, HTTP/JSON transport only       |
//! | [`agentd-grpc`](../../agentd-grpc) | Env-driven agent, gRPC transport only            |
//! | [`tls-roundtrip`](../../tls-roundtrip) | Minimal mTLS demo of `solti-tls` alone       |

mod config;

use std::sync::Arc;

use clap::Parser;
use tonic::transport::Server;
use tracing::info;

use solti_api::{
    API_VERSION, GrpcApi, HttpApi, SupervisorApiAdapter, http_metrics_middleware,
    to_tonic_server_tls,
};
use solti_core::{StateConfig, SupervisorApi};
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    AdmissionPolicy, AgentId, Flag, RestartPolicy, RunnerEnv, SubprocessMode, SubprocessSpec,
    TaskEnv, TaskKind, TaskSpec,
};
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
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

use crate::config::{Config, Transport};

use axum_server::tls_rustls::RustlsConfig;

const METRICS_ADDR: &str = "0.0.0.0:9090";

/// Solti reference agent that connects to a Podium control-plane.
#[derive(Parser)]
#[command(name = "podium-agent", version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "config.toml")]
    config: std::path::PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_local_offset();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    tokio::runtime::Runtime::new()?.block_on(async_main(cfg))
}

async fn async_main(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    init_logger(&LoggerConfig {
        level: LoggerLevel::new("info")?,
        tz: LoggerTimeZone::Local,
        ..Default::default()
    })?;

    // --- Metrics registry & subscribers ---
    let registry = Arc::new(Registry::new());
    let metrics = PrometheusMetrics::new(registry.clone())?;
    let prom_subscriber = PrometheusSubscriber::new(registry.clone())?;
    register_process_collector(&registry)?;
    register_build_info(
        &registry,
        &[("agent", "podium"), ("version", env!("CARGO_PKG_VERSION"))],
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

    // --- Optional local demo task; the agent does something on its own ---
    if cfg.task.enabled {
        let kind = TaskKind::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: cfg.task.command.clone(),
                args: cfg.task.args.clone(),
            },
            TaskEnv::new(),
            None,
            Flag::enabled(),
        ));
        let spec = TaskSpec::builder(cfg.task.name.as_str(), kind, 86_400_000u64) // per-attempt timeout (ms): 24h
            .restart(RestartPolicy::always())
            .admission(AdmissionPolicy::Replace)
            .build()?;
        supervisor.submit(&spec).await?;
        info!("submitted demo task '{}'", cfg.task.name);
    }

    // --- Optional auth & TLS, from config ---
    let token = cfg.token()?;
    let server_tls = cfg.server_tls()?; // agent API server TLS (podium → agent)
    let client_tls = cfg.client_tls()?; // discovery client TLS (agent → podium)

    // --- Discovery: agent → control plane ---
    let (disc_transport, advertise) = match cfg.transport {
        Transport::Http => {
            let scheme = if cfg.tls.enabled { "https" } else { "http" };
            (
                DiscoveryTransport::Http,
                format!("{scheme}://{}", cfg.agent.advertise),
            )
        }
        Transport::Grpc => (DiscoveryTransport::Grpc, cfg.agent.advertise.clone()),
    };
    let discover_metrics = Arc::new(PrometheusDiscoverMetrics::new(registry.clone())?);
    let mut discover = DiscoverConfig::builder(
        AgentId::new(cfg.agent.id.as_str()),
        cfg.agent.name.as_str(),
        advertise,
        cfg.control_plane.endpoint.as_str(),
        disc_transport,
        cfg.control_plane.heartbeat_ms,
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

    let listen = cfg.agent.listen.clone();
    let secure = server_tls.is_some();

    match cfg.transport {
        // ---------------- HTTP (axum / axum-server) ----------------
        Transport::Http => {
            let mut api = HttpApi::new(handler);
            if let Some(t) = token {
                api = api.with_auth(t);
            }
            let app = api.router().layer(axum::middleware::from_fn_with_state(
                api_metrics,
                http_metrics_middleware,
            ));

            let scheme = if secure { "https" } else { "http" };
            info!(
                "{scheme} {listen}  →  heartbeat {}",
                cfg.control_plane.endpoint
            );

            match server_tls {
                Some(tls) => {
                    let rustls = Arc::new(tls.into_rustls_config()?);
                    let srv = axum_server::Handle::new();
                    let srv2 = srv.clone();
                    tokio::spawn(async move {
                        shutdown_signal().await;
                        srv2.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
                    });
                    axum_server::bind_rustls(listen.parse()?, RustlsConfig::from_config(rustls))
                        .handle(srv)
                        .serve(app.into_make_service())
                        .await?;
                }
                None => {
                    let listener = tokio::net::TcpListener::bind(&listen).await?;
                    axum::serve(listener, app)
                        .with_graceful_shutdown(shutdown_signal())
                        .await?;
                }
            }
        }

        // ---------------- gRPC (tonic) ----------------
        Transport::Grpc => {
            let mut builder = Server::builder();
            if let Some(tls) = server_tls {
                builder = builder.tls_config(to_tonic_server_tls(&tls)?)?;
            }
            let scheme = if secure { "grpcs" } else { "grpc" };
            info!(
                "{scheme} {listen}  →  heartbeat {}",
                cfg.control_plane.endpoint
            );
            let addr = listen.parse()?;

            let mut api = GrpcApi::new(handler).with_metrics(api_metrics);
            if let Some(t) = token {
                api = api.with_auth(t);
            }
            builder
                .add_service(api.server())
                .serve_with_shutdown(addr, shutdown_signal())
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
