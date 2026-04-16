//! # agentd - Solti Agent (API v1)
//!
//! Reference agent binary that connects to [Podium](https://github.com/soltiHQ/podium) control-plane.
//!
//! The agent exposes an HTTP API for task management and periodically sends heartbeats to the control-plane via solti-discover.
//! Tasks are submitted remotely by the control-plane or directly via the API.
//!
//! ## What this shows
//!
//! - `SupervisorApi` - task lifecycle management with a runner and state tracking.
//! - `StateConfig` - sweep is always-on; prevents unbounded memory growth by periodically removing completed runs and terminal tasks that exceed their TTL.
//! - `solti_discover::sync()` - periodic heartbeat to the control-plane.
//! - `solti_api::API_VERSION` - protocol version reported to the control-plane.
//! - `HttpApi` - HTTP/JSON API for submitting, querying, canceling, and deleting tasks.
//! - `init_local_offset()` + `timezone_sync()` - local timezone in structured logs.
//! - `TracingEventSubscriber` - logs task lifecycle events.
//!
//! ## SDK crates used
//!
//! | Crate            | Role                                                   |
//! |------------------|--------------------------------------------------------|
//! | `solti-model`    | `AgentId`, domain types                                |
//! | `solti-runner`   | `RunnerRouter` - runner registration                   |
//! | `solti-exec`     | `register_subprocess_runner` - subprocess backend      |
//! | `solti-core`     | `SupervisorApi`, `StateConfig`                         |
//! | `solti-api`      | `HttpApi`, `SupervisorApiAdapter`, `API_VERSION`       |
//! | `solti-discover` | `DiscoverConfig`, `sync()` - control-plane heartbeat   |
//! | `solti-observe`  | Logger, `timezone_sync`, `TracingEventSubscriber`      |
//!
//! ## API reference
//!
//! See [`api_v1.md`](../../crates/solti-api/api_v1.md) for the full HTTP/gRPC endpoint reference with curl/grpcurl examples.
//!
//! ## gRPC transport
//!
//! This example uses HTTP by default. To switch to gRPC:
//!
//! 1. In `Cargo.toml` change the `solti-api` feature from `http` to `grpc`, and uncomment the `tonic` dependency.
//! 2. Replace the HTTP server block with:
//!
//! ```text
//! use solti_api::{SoltiApiServer, SoltiApiService};
//! use tonic::transport::Server;
//!
//! let service = SoltiApiService::new(handler);
//! Server::builder()
//!     .add_service(SoltiApiServer::new(service))
//!     .serve("[::1]:50051".parse()?)
//!     .await?;
//! ```
//!
//! ## Run
//!
//! ```bash
//! cargo run -p agentd
//! # Endpoints:
//! #   http://localhost:8085/api/v1/tasks  - task API
//! # Requires Podium control-plane on localhost:8082 for discovery.
//! # Without it the agent still works - heartbeat retries with backoff.
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use solti_api::{API_VERSION, HttpApi, SupervisorApiAdapter};
use solti_core::{StateConfig, SupervisorApi};
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::AgentId;
use solti_observe::{
    LoggerConfig, LoggerLevel, LoggerTimeZone, TracingEventSubscriber, init_local_offset,
    init_logger, timezone_sync,
};
use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

const ADDR: &str = "0.0.0.0:8085";
const CONTROL_PLANE: &str = "http://localhost:8082";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Capture UTC offset before tokio spawns threads.
    init_local_offset();

    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Structured logger with local timezone
    let log_cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        tz: LoggerTimeZone::Local,
        ..Default::default()
    };
    init_logger(&log_cfg)?;
    info!("logger initialized");

    // 2) Runner - standard subprocess backend
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;
    info!("subprocess runner registered");

    // 3) Supervisor
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingEventSubscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
        StateConfig::default(),
    )
    .await?;

    // 4) Timezone sync - refreshes UTC offset for log timestamps
    let (tz_task, tz_spec) = timezone_sync();
    supervisor.submit_with_task(tz_task, &tz_spec).await?;
    info!("timezone sync started");

    // 5) Discovery - periodic heartbeat to Podium control-plane
    let discover_config = DiscoverConfig::builder(
        AgentId::new("agentd-001"),
        "agentd",
        format!("http://{ADDR}"),
        CONTROL_PLANE,
        DiscoveryTransport::Http,
        10_000,
        API_VERSION,
    )
    .metadata(HashMap::from([
        ("region".into(), "us-east-1".into()),
        ("role".into(), "worker".into()),
    ]))
    .build()?;
    let (sync_task, sync_spec) = solti_discover::sync(discover_config)?;
    supervisor.submit_with_task(sync_task, &sync_spec).await?;
    info!("discovery heartbeat started (control_plane={CONTROL_PLANE}, api_version={API_VERSION})");

    // 6) HTTP API
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let app = HttpApi::new(handler).router();

    // ── Uncomment for gRPC transport (see doc header for details) ──
    // use solti_api::{SoltiApiServer, SoltiApiService};
    // use tonic::transport::Server;
    // let service = SoltiApiService::new(handler);
    // Server::builder()
    //     .add_service(SoltiApiServer::new(service))
    //     .serve("[::1]:50051".parse()?)
    //     .await?;

    let listener = tokio::net::TcpListener::bind(ADDR).await?;
    info!("HTTP API: http://{ADDR}/api/v1/tasks");
    info!("press Ctrl+C to stop");
    axum::serve(listener, app).await?;

    Ok(())
}
