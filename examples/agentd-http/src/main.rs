//! Reference Solti agent — HTTP/JSON transport.
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
//! - API  — `http://localhost:8085/api/v1/tasks`
//! - Heartbeat — `http://localhost:8082` (Podium HTTP discovery)
//! - Override the CP endpoint via `CONTROL_PLANE=http://host:port`.
//!
//! Running without a reachable Podium is fine:
//! the heartbeat task retries with backoff, the local API keeps accepting submissions.
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
        AgentId::new("agentd-http-001"),
        "agentd-http",
        format!("http://{ADDR}"),
        &control_plane,
        DiscoveryTransport::Http,
        10_000,
        API_VERSION,
    )
    .build()?;
    let (sync_task, sync_spec) = solti_discover::sync(discover_config)?;
    supervisor.submit_with_task(sync_task, &sync_spec).await?;

    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let app = HttpApi::new(handler).router();

    let listener = tokio::net::TcpListener::bind(ADDR).await?;
    info!("HTTP http://{ADDR}  →  heartbeat {control_plane}");
    axum::serve(listener, app).await?;
    Ok(())
}
