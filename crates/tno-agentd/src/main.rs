use std::{sync::Arc, time::Duration};

use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use tno_core::{RunnerRouter, SupervisorApi, TaskPolicy};
use tno_exec::subprocess::SubprocessRunner;
use tno_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger, timezone_sync};

use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, Env, Flag, JitterStrategy, RestartStrategy,
    TaskKind,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // 1) logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("debug")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) Subscribe
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Subscriber)];

    // 3) Router
    let mut router = RunnerRouter::new();
    router.register(Arc::new(SubprocessRunner::new()));

    // 4) SupervisorApi
    let api = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;

    // 5) Internal timezone-sync
    let (tz_task, tz_spec) = timezone_sync();
    let tz_policy = TaskPolicy::from_spec(&tz_spec);
    api.submit_with_task(tz_task, &tz_policy).await?;

    // 6) subprocess: `ls /tmp` / CreateSpec + TaskKind::Subprocess
    let ls_spec = CreateSpec {
        slot: "demo-ls-tmp".to_string(),
        kind: TaskKind::Subprocess {
            command: "ls".into(),
            args: vec!["/tmp".into()],
            env: Env::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::Never,
        backoff: BackoffStrategy {
            jitter: JitterStrategy::None,
            first_ms: 0,
            max_ms: 0,
            factor: 1.0,
        },
        admission: AdmissionStrategy::DropIfRunning,
    };

    api.submit(&ls_spec).await?;

    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok(())
}
