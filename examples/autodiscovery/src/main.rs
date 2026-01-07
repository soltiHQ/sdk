use std::sync::Arc;

use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use tno_api::{AutodiscoveryConfig, build_heartbeat_task};
use tno_core::{RunnerRouter, SupervisorApi, TaskPolicy};
use tno_exec::subprocess::register_subprocess_runner;
use tno_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) Router + runners
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default-runner")?;
    info!("registered default subprocess runner");

    // 3) Supervisor
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Subscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;
    info!("supervisor ready");

    // 4) Autodiscovery configuration
    let autodiscovery_config = AutodiscoveryConfig {
        agent_id: "demo-agent-1".to_string(),
        agent_endpoint: "http://localhost:8080".to_string(),
        lighthouse_endpoint: "http://localhost:50051".to_string(),
        heartbeat_interval_ms: 10_000,
        heartbeat_timeout_ms: 5_000,
    };
    autodiscovery_config.validate()?;
    info!(
        "autodiscovery configured: lighthouse={}",
        autodiscovery_config.lighthouse_endpoint
    );

    // 5) Submit heartbeat task
    let (heartbeat_task, heartbeat_spec) = build_heartbeat_task(autodiscovery_config);
    let policy = TaskPolicy::from_spec(&heartbeat_spec);
    let heartbeat_id = supervisor.submit_with_task(heartbeat_task, &policy).await?;
    info!("heartbeat task submitted: {}", heartbeat_id);

    // 6) Keep running
    info!("agent is running with autodiscovery enabled");
    info!("press Ctrl+C to stop");

    tokio::signal::ctrl_c().await?;
    info!("shutting down...");

    Ok(())
}
