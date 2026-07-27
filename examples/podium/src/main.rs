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

use std::{net::SocketAddr, sync::Arc};

use clap::Parser;
use tonic::transport::Server;
use tracing::info;

use solti::api::{
    API_VERSION, ApiMetricsBackend, GrpcApi, HttpApi, SupervisorApiAdapter, to_tonic_server_tls,
};
use solti::core::{StateConfig, SupervisorApi};
use solti::discover::{
    self, AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig,
    DiscoveryTransport, MonotonicUptime,
};
use solti::exec::subprocess::register_subprocess_runner;
use solti::model::{
    AdmissionPolicy, AgentId, Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv,
    TaskManifest, TaskSpec, TaskWorkload,
};
use solti::observe::{LoggerConfig, LoggerLevel, LoggerTimeZone, init_logger, timezone_sync};
use solti::prometheus::{
    PrometheusApiMetrics, PrometheusCoreStateCollector, PrometheusDiscoverMetrics,
    PrometheusRunnerMetrics, PrometheusTaskvisorSubscriber, Registry, register_build_info,
    register_process_collector, server as metrics_server,
};
use solti::runner::{BuildContext, RunnerEnv, RunnerRouter};
use solti::taskvisor::{ControllerConfig, Subscribe, SupervisorConfig, TracingBridge};

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
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    tokio::runtime::Runtime::new()?.block_on(async_main(cfg))
}

async fn async_main(cfg: Config) -> Result<(), Box<dyn std::error::Error>> {
    let uptime = Arc::new(MonotonicUptime::new());

    init_logger(&LoggerConfig {
        level: LoggerLevel::new("info")?,
        timezone: LoggerTimeZone::Local,
        ..Default::default()
    })?;

    // --- Metrics registry & subscribers ---
    let registry = Arc::new(Registry::new());
    let metrics = PrometheusRunnerMetrics::new(&registry)?;
    let prom_subscriber = PrometheusTaskvisorSubscriber::new(&registry)?;
    register_process_collector(&registry)?;
    register_build_info(
        &registry,
        &[("agent", "podium"), ("version", env!("CARGO_PKG_VERSION"))],
    )?;

    // Runner: executes TaskSpec bodies (subprocess here).
    let ctx = BuildContext::new(RunnerEnv::default(), Arc::new(metrics));
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default")?;
    let agent_capabilities = router.capabilities();

    // Supervisor: owns every task, applies restart / backoff, fans events to subscribers.
    let subscribers: Vec<Arc<dyn Subscribe>> =
        vec![Arc::new(TracingBridge), Arc::new(prom_subscriber)];
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

    // --- Optional local demo task; the agent does something on its own ---
    if cfg.task.enabled {
        let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: cfg.task.command.clone(),
                args: cfg.task.args.clone(),
            },
            TaskEnv::new(),
            None,
            Flag::enabled(),
        ));
        let spec = TaskSpec::builder(cfg.task.name.as_str(), workload, 86_400_000u64) // per-attempt timeout (ms): 24h
            .restart(RestartPolicy::always())
            .admission(AdmissionPolicy::Replace)
            .build()?;
        supervisor
            .create_task(TaskManifest::new(cfg.task.name.as_str(), spec)?)
            .await?;
        info!("submitted demo task '{}'", cfg.task.name);
    }

    // --- Optional auth & TLS, from config ---
    let token = cfg.token()?;
    let server_tls = cfg.server_tls()?; // agent API server TLS (podium → agent)
    let client_tls = cfg.client_tls()?; // discovery client TLS (agent → podium)

    // --- Discovery: agent → control plane ---
    let (agent_endpoint_type, discovery_transport, advertise) = match cfg.transport {
        Transport::Http => {
            let scheme = if cfg.tls.enabled { "https" } else { "http" };
            (
                AgentEndpointType::Http,
                DiscoveryTransport::Http,
                format!("{scheme}://{}", cfg.agent.advertise),
            )
        }
        Transport::Grpc => (
            AgentEndpointType::Grpc,
            DiscoveryTransport::Grpc,
            cfg.agent.advertise.clone(),
        ),
    };
    let discover_metrics = Arc::new(PrometheusDiscoverMetrics::new(&registry)?);
    let mut discover = DiscoverConfig::builder(
        AgentId::new(cfg.agent.id.as_str())?,
        cfg.agent.name.as_str(),
        AgentEndpoint::new(advertise, agent_endpoint_type, API_VERSION)?,
        ControlPlaneEndpoint::new(cfg.control_plane.endpoint.as_str(), discovery_transport)?,
        cfg.control_plane.heartbeat_ms,
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

    let listen = cfg.agent.listen.clone();
    let secure = server_tls.is_some();

    match cfg.transport {
        // ---------------- HTTP (axum / axum-server) ----------------
        Transport::Http => {
            let mut api = HttpApi::new(handler).with_metrics(api_metrics);
            if let Some(t) = token {
                api = api.with_auth(t);
            }
            let app = api.router();

            let scheme = if secure { "https" } else { "http" };
            info!(
                "{scheme} {listen}  →  heartbeat {}",
                cfg.control_plane.endpoint
            );

            match server_tls {
                Some(tls) => {
                    let mut rustls = tls.into_rustls_config()?;
                    rustls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
                    let rustls = Arc::new(rustls);
                    let addr: SocketAddr = listen.parse()?;
                    let srv = axum_server::Handle::<SocketAddr>::new();
                    let srv2 = srv.clone();
                    tokio::spawn(async move {
                        shutdown_signal().await;
                        srv2.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
                    });
                    axum_server::bind_rustls(addr, RustlsConfig::from_config(rustls))
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
                builder = builder.tls_config(to_tonic_server_tls(tls)?)?;
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
