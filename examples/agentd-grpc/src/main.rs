//! Reference Solti agent — gRPC transport.
//!
//! A minimal task-execution agent: accepts `TaskSpec` submissions via the `solti.v1.SoltiApi` gRPC service,
//! runs them as subprocesses, reports status back through the same service.
//!
//! Optionally heartbeats to a [Podium](https://github.com/soltiHQ/podium) control-plane so the CP can push specs and collect state remotely.
//!
//! ```bash
//! cargo run -p agentd-grpc
//! ```
//!
//! Defaults:
//! - API: `[::]:50052` (`solti.v1.SoltiApi`)
//! - Heartbeat — `localhost:50051` (Podium grpc-discovery)
//! - Override the CP endpoint via `CONTROL_PLANE=host:port`.
//!
//! Running without a reachable Podium is fine:
//! the heartbeat task retries with backoff, the local API keeps accepting submissions.
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
//! ```
//!
//! Proto contract + full RPC surface: [`api_v1.md`](../../../crates/solti-api/api_v1.md).
//! gRPC status codes, message-size caps, error envelope — all described there.

use std::sync::Arc;

use tonic::transport::Server;
use tracing::info;

use solti_api::{API_VERSION, SupervisorApiAdapter, build_grpc_server};
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

const ADDR: &str = "[::]:50052";
const ADVERTISED: &str = "localhost:50052";
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

    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;

    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(TracingEventSubscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
        StateConfig::default(),
    )
    .await?;

    let (tz_task, tz_spec) = timezone_sync();
    supervisor.submit_with_task(tz_task, &tz_spec).await?;

    let control_plane =
        std::env::var("CONTROL_PLANE").unwrap_or_else(|_| CONTROL_PLANE_DEFAULT.to_string());
    let discover_config = DiscoverConfig::builder(
        AgentId::new("agentd-grpc-001"),
        "agentd-grpc",
        ADVERTISED,
        &control_plane,
        DiscoveryTransport::Grpc,
        10_000,
        API_VERSION,
    )
    .build()?;
    let (sync_task, sync_spec) = solti_discover::sync(discover_config)?;
    supervisor.submit_with_task(sync_task, &sync_spec).await?;

    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let service = build_grpc_server(handler);

    info!("gRPC {ADDR}  →  heartbeat {control_plane}");
    Server::builder()
        .add_service(service)
        .serve(ADDR.parse()?)
        .await?;
    Ok(())
}
