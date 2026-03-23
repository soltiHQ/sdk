use std::sync::Arc;

use axum::routing::get;
use tracing::info;

use solti_api::{HttpApi, SupervisorApiAdapter};
use solti_core::{BuildContext, RunnerRouter, SupervisorApi};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    AdmissionPolicy, BackoffPolicy, Flag, JitterPolicy, RestartPolicy, RunnerEnv, SubprocessMode,
    TaskEnv, TaskKind, TaskSpec,
};
use solti_observe::{
    LoggerConfig, LoggerLevel, TracingEventSubscriber, init_logger, timezone_sync,
};
use solti_prometheus::{PrometheusMetrics, PrometheusSubscriber};
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Initialize logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) Create shared Prometheus registry and metrics
    let registry = Arc::new(prometheus::Registry::new());
    let metrics = PrometheusMetrics::new_with_registry(registry.clone())?;
    let metrics_handle = Arc::new(metrics.clone());
    let prom_subscriber = PrometheusSubscriber::new(registry)?;
    info!("prometheus metrics initialized");

    // 3) Setup router with subprocess runner
    let ctx = BuildContext::new(RunnerEnv::default(), metrics_handle);
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default-runner")?;
    info!("registered default subprocess runner");

    // 4) Create supervisor
    let subscribers: Vec<Arc<dyn Subscribe>> =
        vec![Arc::new(TracingEventSubscriber), Arc::new(prom_subscriber)];
    let supervisor = SupervisorApi::new(
        SupervisorConfig::default(),
        ControllerConfig::default(),
        subscribers,
        router,
    )
    .await?;
    info!("supervisor ready");

    // 5) Submit timezone sync task
    let (tz_task, tz_spec) = timezone_sync();
    supervisor.submit_with_task(tz_task, &tz_spec).await?;
    info!("timezone sync task submitted");

    // 6) Submit demo periodic tasks
    submit_demo_tasks(&supervisor).await?;
    info!("demo periodic tasks submitted");

    // 7) Create API handler and HTTP service
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let http_api = HttpApi::new(handler);
    let app = http_api.router();

    // 8) Add /metrics endpoint
    let metrics_clone = metrics.clone();
    let app = app.route(
        "/metrics",
        get(move || metrics_handler(metrics_clone.clone())),
    );

    // 9) Start HTTP server
    let addr = "0.0.0.0:8080";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("starting HTTP server on http://{}", addr);
    info!("API: http://{}/api/v1/tasks", addr);
    info!("Metrics: http://{}/metrics", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

/// Prometheus metrics handler
async fn metrics_handler(metrics: PrometheusMetrics) -> String {
    use solti_prometheus::{Encoder, TextEncoder};

    let families = metrics.gather();
    let encoder = TextEncoder::new();
    let mut buffer = vec![];

    encoder.encode(&families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Submit demo periodic tasks that run continuously
async fn submit_demo_tasks(api: &SupervisorApi) -> Result<(), Box<dyn std::error::Error>> {
    // Task 1: Print date every 10 seconds
    let date_spec = TaskSpec::builder(
        "periodic-date",
        TaskKind::Subprocess {
            mode: SubprocessMode::Command {
                command: "date".into(),
                args: vec!["+%Y-%m-%d %H:%M:%S".into()],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        5_000_u64,
    )
    .restart(RestartPolicy::periodic(10_000)) // Every 10 seconds
    .backoff(BackoffPolicy {
        jitter: JitterPolicy::None,
        first_ms: 1_000,
        max_ms: 5_000,
        factor: 2.0,
    })
    .build()
    .unwrap();

    // Task 2: Print uptime every 30 seconds
    let uptime_spec = TaskSpec::builder(
        "periodic-uptime",
        TaskKind::Subprocess {
            mode: SubprocessMode::Command {
                command: "uptime".into(),
                args: vec![],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        5_000_u64,
    )
    .restart(RestartPolicy::periodic(30_000)) // Every 30 seconds
    .backoff(BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: 1_000,
        max_ms: 5_000,
        factor: 2.0,
    })
    .build()
    .unwrap();

    // Task 3: Echo message every 5 seconds
    let echo_spec = TaskSpec::builder(
        "periodic-echo",
        TaskKind::Subprocess {
            mode: SubprocessMode::Command {
                command: "echo".into(),
                args: vec!["Hello from solti periodic task!".into()],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        5_000_u64,
    )
    .restart(RestartPolicy::periodic(5_000)) // Every 5 seconds
    .backoff(BackoffPolicy {
        jitter: JitterPolicy::Full,
        first_ms: 500,
        max_ms: 2_000,
        factor: 1.5,
    })
    .admission(AdmissionPolicy::Replace)
    .build()
    .unwrap();

    let date_id = api.submit(&date_spec).await?;
    info!("submitted periodic date task: {}", date_id);

    let uptime_id = api.submit(&uptime_spec).await?;
    info!("submitted periodic uptime task: {}", uptime_id);

    let echo_id = api.submit(&echo_spec).await?;
    info!("submitted periodic echo task: {}", echo_id);

    Ok(())
}
