use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use tno_core::{RunnerRouter, SupervisorApi, TaskPolicy};
use tno_discover::{DiscoverConfig, DiscoveryTransport};
use tno_exec::subprocess::register_subprocess_runner;
use tno_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 2) Logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 3) Router + runners
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default-runner")?;
    info!("registered default subprocess runner");

    // 4) Supervisor
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Subscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;
    info!("supervisor ready");

    // 5) Discovery configuration
    let discover_config = DiscoverConfig {
        name: "demo-agent".to_string(),
        control_plane_endpoint: "http://localhost:8082".to_string(),
        agent_endpoint: "http://localhost:8085".to_string(),
        transport: DiscoveryTransport::Http,
        metadata: HashMap::new(),
        delay_ms: 10_000,
    };
    info!(
        "discovery configured: control_plane={}, agent={}, transport={:?}",
        discover_config.control_plane_endpoint,
        discover_config.agent_endpoint,
        discover_config.transport
    );

    // 6) Submit sync task
    let (sync_task, sync_spec) = tno_discover::sync(discover_config);
    let policy = TaskPolicy::from_spec(&sync_spec);
    let sync_id = supervisor.submit_with_task(sync_task, &policy).await?;
    info!("sync task submitted: {}", sync_id);

    // 7) Keep running
    info!("agent is running with discovery enabled");
    info!("press Ctrl+C to stop");

    tokio::signal::ctrl_c().await?;
    info!("shutting down...");

    Ok(())
}
