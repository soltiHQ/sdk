use std::{sync::Arc, time::Duration};

use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use tno_core::{RunnerRouter, SupervisorApi, TaskPolicy};
use tno_exec::subprocess::register_subprocess_runner;
use tno_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger, timezone_sync};

use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, Env, Flag, JitterStrategy, Labels,
    RestartStrategy, TaskKind,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // 1) logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) subscribers
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Subscriber)];

    // 3) router + runners
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "runner").expect("message");
    register_subprocess_runner(&mut router, "etc").expect("message");

    // 4) SupervisorApi
    let api = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;

    // 5) internal timezone-sync
    let (tz_task, tz_spec) = timezone_sync();
    let tz_policy = TaskPolicy::from_spec(&tz_spec);
    api.submit_with_task(tz_task, &tz_policy).await?;

    // 6) subprocess
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
        labels: Labels::default(),
    }
    .with_runner_tag("runner");

    // pwd
    let pwd_spec = CreateSpec {
        slot: "demo-pwd-tmp".to_string(),
        kind: TaskKind::Subprocess {
            command: "pwd".into(),
            args: vec![],
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
        labels: Labels::default(),
    }
    .with_runner_tag("runner");

    // date
    let date_spec = CreateSpec {
        slot: "demo-date-tmp".to_string(),
        kind: TaskKind::Subprocess {
            command: "date".into(),
            args: vec![],
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
        labels: Labels::default(),
    }
    .with_runner_tag("runner");

    // sleep 3
    let sleep_spec = CreateSpec {
        slot: "demo-sleep-tmp".to_string(),
        kind: TaskKind::Subprocess {
            command: "sleep".into(),
            args: vec!["3".into()],
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
        labels: Labels::default(),
    }
    .with_runner_tag("etc");

    api.submit(&sleep_spec).await?;
    api.submit(&ls_spec).await?;
    api.submit(&pwd_spec).await?;
    api.submit(&date_spec).await?;

    tokio::time::sleep(Duration::from_secs(5)).await;
    Ok(())
}
