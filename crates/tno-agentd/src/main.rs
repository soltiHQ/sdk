use std::{sync::Arc, time::Duration};

use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use tno_core::{RunnerRouter, SupervisorApi, TaskPolicy};

use tno_exec::subprocess::SubprocessBackendConfig;
use tno_exec::subprocess::register_subprocess_runner_with_backend;

use tno_exec::{CgroupLimits, CpuMax, LinuxCapability, RlimitConfig, SecurityConfig};

use tno_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger, timezone_sync};

use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, Env, Flag, JitterStrategy, Labels,
    RestartStrategy, TaskKind,
};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    // 1) logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("trace")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) subscribers
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Subscriber)];

    // 3) router + runners with DIFFERENT security profiles
    let mut router = RunnerRouter::new();

    // 3a) Development runner - NO restrictions
    register_subprocess_runner_with_backend(
        &mut router,
        "dev-runner",
        SubprocessBackendConfig::new(),
    )?;
    info!("registered dev-runner (no restrictions)");

    // 3b) Production runner - moderate restrictions
    let prod_backend = SubprocessBackendConfig::new()
        .with_rlimits(RlimitConfig {
            max_open_files: Some(1024),
            max_file_size_bytes: Some(100 * 1024 * 1024), // 100 MB
            disable_core_dumps: true,
        })
        .with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(50_000), // 50% CPU (50ms per 100ms)
                period: 100_000,     // 100ms
            }),
            memory: Some(256 * 1024 * 1024), // 256 MB
            pids: Some(64),                  // max 64 processes
        });
    register_subprocess_runner_with_backend(&mut router, "prod-runner", prod_backend)?;
    info!("registered prod-runner (moderate restrictions)");

    // 3c) Untrusted runner - MAXIMUM security
    let untrusted_backend = SubprocessBackendConfig::new()
        .with_rlimits(RlimitConfig {
            max_open_files: Some(128),
            max_file_size_bytes: Some(10 * 1024 * 1024), // 10 MB only
            disable_core_dumps: true,
        })
        .with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(25_000),
                period: 100_000,
            }),

            memory: Some(64 * 1024 * 1024), pids: Some(16),
        })
        .with_security(SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetBindService],
            no_new_privs: true, // CRITICAL  untrusted code
        });
    register_subprocess_runner_with_backend(&mut router, "untrusted-runner", untrusted_backend)?;
    info!("registered untrusted-runner (MAXIMUM security)");

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

    // 6a) Dev runner
    let ls_spec = CreateSpec {
        slot: "dev-ls-tmp".to_string(),
        kind: TaskKind::Subprocess {
            command: "ls".into(),
            args: vec!["-lah".into(), "/tmp".into()],
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
    .with_runner_tag("dev-runner");

    // 6b) Production runner
    let date_spec = CreateSpec {
        slot: "prod-date".to_string(),
        kind: TaskKind::Subprocess {
            command: "date".into(),
            args: vec!["+%Y-%m-%d %H:%M:%S".into()],
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
    .with_runner_tag("prod-runner");

    // 6c) Untrusted runner
    let sleep_spec = CreateSpec {
        slot: "untrusted-sleep".to_string(),
        kind: TaskKind::Subprocess {
            command: "sleep".into(),
            args: vec!["2".into()],
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
    .with_runner_tag("untrusted-runner");

    // 6d) Untrusted runner
    let stress_spec = CreateSpec {
        slot: "untrusted-stress".to_string(),
        kind: TaskKind::Subprocess {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "for i in $(seq 1 100); do sleep 1 & done; wait".into(),
            ],
            env: Env::default(),
            cwd: None,
            fail_on_non_zero: Flag::disabled(),
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
    .with_runner_tag("untrusted-runner");

    // Submit tasks
    info!("submitting tasks...");
    api.submit(&ls_spec).await?;
    api.submit(&date_spec).await?;
    api.submit(&sleep_spec).await?;
    api.submit(&stress_spec).await?;

    info!("all tasks submitted, waiting for completion...");
    tokio::time::sleep(Duration::from_secs(8)).await;

    info!("demo completed");
    Ok(())
}
