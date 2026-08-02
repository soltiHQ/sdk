//! # HTTP agent with discovery
//!
//! This example assembles an inbound Task API and an outbound discovery heartbeat.
//! The runner registry is the source of the capabilities sent to the control plane.
//!
//! This example shows:
//!
//! - subprocess runner registration;
//! - automatic capability snapshot creation;
//! - an HTTP Task API backed by core;
//! - a supervised embedded discovery task;
//! - independent inbound and outbound endpoints;
//! - graceful shutdown of transport and SDK-owned workers.
//!
//! ```text
//! RunnerRouter ──► capabilities ───────────────┐
//!      ▼                                       ▼
//! SupervisorApi ◄── embedded discovery TaskRef + manifest
//!      │                                       │ heartbeat
//!      ▼                                       ▼
//! HTTP Task API                         external control plane
//! inbound :8085                         outbound sync endpoint
//! ```
//!
//! The control plane must implement discovery HTTP v1.
//! `SOLTI_CONTROL_PLANE` defaults to `http://127.0.0.1:8090`.
//!
//! Run with `cargo run -p solti --example agent_http_discovery --features api-core-adapter,api-http,discover-http,exec-subprocess`.

use std::sync::Arc;

use solti::{
    api::{API_VERSION, HTTP_API_ROOT, HttpApi, SupervisorApiAdapter, axum::serve},
    core::SupervisorApi,
    discover::{
        AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DISCOVERY_HTTP_SYNC_PATH,
        DiscoverConfig, DiscoveryTransport, MonotonicUptime, sync,
    },
    exec::subprocess::register_subprocess_runner,
    model::AgentId,
    runner::RunnerRouter,
};
use tokio::net::TcpListener;

const API_ADDRESS: &str = "127.0.0.1:8085";
const DEFAULT_CONTROL_PLANE: &str = "http://127.0.0.1:8090";

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!(
        r#"
solti: discovered HTTP agent

  control plane ◄── discovery heartbeat ── embedded task ──┐
  API clients ──► HTTP Task API ──► adapter ──► core ◄─────┤
                                                  ▼        │
                                            runner router ─┘ capabilities
"#
    );
    println!(
        "[purpose] Advertise the exact runner capabilities served by one live HTTP task agent."
    );

    let listener = TcpListener::bind(API_ADDRESS).await?;
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;
    let capabilities = router.capabilities();
    println!(
        "[runner] Registered {} runner capability with {} workload GVK.",
        capabilities.runners().len(),
        capabilities.runners()[0].workload_types().len(),
    );

    let supervisor = Arc::new(SupervisorApi::builder(router).start().await?);
    let control_plane =
        std::env::var("SOLTI_CONTROL_PLANE").unwrap_or_else(|_| DEFAULT_CONTROL_PLANE.into());
    let advertised = format!("http://{API_ADDRESS}");
    let revision = format!(
        "discovered-agent@{}|control-plane={control_plane}",
        env!("CARGO_PKG_VERSION")
    );
    let config = DiscoverConfig::builder(
        AgentId::new("umbrella-example-agent")?,
        "Umbrella example agent",
        AgentEndpoint::new(&advertised, AgentEndpointType::Http, API_VERSION)?,
        ControlPlaneEndpoint::new(&control_plane, DiscoveryTransport::Http)?,
        10_000,
        revision,
    )
    .capabilities(capabilities)
    .build()?;
    let (manifest, task_ref) = sync(config, Arc::new(MonotonicUptime::new()))?;
    let discovery_name = manifest.name().clone();
    supervisor.create_embedded_task(manifest, task_ref).await?;
    println!("[discovery] Supervised embedded task={discovery_name}.");
    println!(
        "[discovery] controlPlane={control_plane}, path={DISCOVERY_HTTP_SYNC_PATH}, interval=10s."
    );

    let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
    let app = HttpApi::new(handler).router();
    println!("[api] Task API: {advertised}{HTTP_API_ROOT}");
    println!("[api] Embedded discovery state is hidden by the public adapter.");
    println!("[shutdown] Press Ctrl-C to stop intake and supervised tasks.");

    let server_result = serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    let shutdown_result = supervisor.shutdown().await;
    server_result?;
    shutdown_result?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to install Ctrl-C handler: {error}");
    }
}
