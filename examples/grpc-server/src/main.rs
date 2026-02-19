use std::sync::Arc;

use tonic::transport::Server;
use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use solti_api::{SupervisorApiAdapter, SoltiApiServer, SoltiApiService};
use solti_core::{RunnerRouter, SupervisorApi};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, Flag, JitterStrategy, RestartStrategy,
    RunnerLabels, TaskEnv, TaskKind,
};
use solti_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger, timezone_sync};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Initialize logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) Setup router with subprocess runner
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default-runner")?;
    info!("registered default subprocess runner");

    // 3) Create supervisor
    let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(Subscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;
    info!("supervisor ready");

    // 4) Submit timezone sync task
    let (tz_task, tz_spec) = timezone_sync();
    let tz_policy = solti_core::TaskPolicy::from_spec(&tz_spec);
    supervisor.submit_with_task(tz_task, &tz_policy).await?;
    info!("timezone sync task submitted");

    // 5) Submit demo periodic tasks
    submit_demo_tasks(&supervisor).await?;
    info!("demo periodic tasks submitted");

    // 6) Create API handler and gRPC service
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let service = SoltiApiService::new(handler);

    // 7) Start gRPC server
    let addr = "[::1]:50051".parse()?;
    info!("starting gRPC server on {}", addr);
    info!("use grpcurl to interact with the API");

    Server::builder()
        .add_service(SoltiApiServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

/// Submit demo periodic tasks that run continuously
async fn submit_demo_tasks(api: &SupervisorApi) -> Result<(), Box<dyn std::error::Error>> {
    // Task 1: Print date every 10 seconds
    let date_spec = CreateSpec {
        slot: "periodic-date".to_string(),
        kind: TaskKind::Subprocess {
            command: "date".into(),
            args: vec!["+%Y-%m-%d %H:%M:%S".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::periodic(10_000), // Every 10 seconds
        backoff: BackoffStrategy {
            jitter: JitterStrategy::None,
            first_ms: 1_000,
            max_ms: 5_000,
            factor: 2.0,
        },
        admission: AdmissionStrategy::DropIfRunning,
        labels: RunnerLabels::default(),
    };

    // Task 2: Print uptime every 30 seconds
    let uptime_spec = CreateSpec {
        slot: "periodic-uptime".to_string(),
        kind: TaskKind::Subprocess {
            command: "uptime".into(),
            args: vec![],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::periodic(30_000), // Every 30 seconds
        backoff: BackoffStrategy {
            jitter: JitterStrategy::Equal,
            first_ms: 1_000,
            max_ms: 5_000,
            factor: 2.0,
        },
        admission: AdmissionStrategy::DropIfRunning,
        labels: RunnerLabels::default(),
    };

    // Task 3: Echo message every 5 seconds
    let echo_spec = CreateSpec {
        slot: "periodic-echo".to_string(),
        kind: TaskKind::Subprocess {
            command: "echo".into(),
            args: vec!["Hello from solti periodic task!".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::periodic(5_000), // Every 5 seconds
        backoff: BackoffStrategy {
            jitter: JitterStrategy::Full,
            first_ms: 500,
            max_ms: 2_000,
            factor: 1.5,
        },
        admission: AdmissionStrategy::Replace,
        labels: RunnerLabels::default(),
    };

    let date_id = api.submit(&date_spec).await?;
    info!("submitted periodic date task: {}", date_id);

    let uptime_id = api.submit(&uptime_spec).await?;
    info!("submitted periodic uptime task: {}", uptime_id);

    let echo_id = api.submit(&echo_spec).await?;
    info!("submitted periodic echo task: {}", echo_id);

    Ok(())
}
