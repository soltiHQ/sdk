use std::collections::HashMap;
use std::sync::Arc;

use axum::routing::get;
use tracing::info;

use solti_api::{HttpApi, SupervisorApiAdapter};
use solti_core::{BuildContext, RunnerRouter, SupervisorApi};
use solti_discover::{DiscoverConfig, DiscoveryTransport};
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    AdmissionPolicy, AgentId, BackoffPolicy, Flag, JitterPolicy, RestartPolicy, RunnerEnv,
    SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec,
};
use solti_observe::{
    LoggerConfig, LoggerLevel, LoggerTimeZone, TracingEventSubscriber, init_local_offset,
    init_logger, timezone_sync,
};
use solti_prometheus::{PrometheusMetrics, PrometheusSubscriber};
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

const AGENT_HTTP_ADDR: &str = "0.0.0.0:8085";
const CONTROL_PLANE_ENDPOINT: &str = "http://localhost:8082";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Must be called before spawning threads (tokio runtime).
    init_local_offset();

    tokio::runtime::Runtime::new()?.block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        tz: LoggerTimeZone::Local,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) Prometheus metrics (shared registry)
    let registry = Arc::new(prometheus::Registry::new());
    let metrics = PrometheusMetrics::new_with_registry(registry.clone())?;
    let metrics_handle = Arc::new(metrics.clone());
    let prom_subscriber = PrometheusSubscriber::new(registry)?;
    info!("prometheus metrics initialized");

    // 3) Router + subprocess runner
    let ctx = BuildContext::new(RunnerEnv::default(), metrics_handle);
    let mut router = RunnerRouter::new().with_context(ctx);
    register_subprocess_runner(&mut router, "default-runner")?;
    info!("registered default subprocess runner");

    // 4) Supervisor
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

    // 5) Internal tasks: timezone sync
    let (tz_task, tz_spec) = timezone_sync();
    supervisor.submit_with_task(tz_task, &tz_spec).await?;
    info!("timezone sync task submitted");

    // 6) Discovery — periodic sync with control plane
    let discover_config = DiscoverConfig {
        agent_id: AgentId::new("demo-agent-001"),
        name: "demo-agent".to_string(),
        control_plane_endpoint: CONTROL_PLANE_ENDPOINT.to_string(),
        agent_endpoint: format!("http://{}", AGENT_HTTP_ADDR),
        transport: DiscoveryTransport::Http,
        metadata: HashMap::from([
            ("region".into(), "us-east-1".into()),
            ("role".into(), "worker".into()),
        ]),
        delay_ms: 10_000,
    };
    info!(
        "discovery: control_plane={}, agent={}, transport={:?}",
        discover_config.control_plane_endpoint,
        discover_config.agent_endpoint,
        discover_config.transport,
    );
    let (sync_task, sync_spec) = solti_discover::sync(discover_config);
    supervisor.submit_with_task(sync_task, &sync_spec).await?;
    info!("discovery sync task submitted");

    // 7) Submit 5 demo background tasks
    submit_background_tasks(&supervisor).await?;

    // 8) HTTP API + metrics
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::new(supervisor)));
    let http_api = HttpApi::new(handler);
    let app = http_api.router();

    let metrics_clone = metrics.clone();
    let app = app.route(
        "/metrics",
        get(move || metrics_handler(metrics_clone.clone())),
    );

    // 9) Start server
    let listener = tokio::net::TcpListener::bind(AGENT_HTTP_ADDR).await?;
    info!("HTTP API:  http://{}/api/v1/tasks", AGENT_HTTP_ADDR);
    info!("Metrics:   http://{}/metrics", AGENT_HTTP_ADDR);
    info!("press Ctrl+C to stop");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn metrics_handler(metrics: PrometheusMetrics) -> String {
    use solti_prometheus::{Encoder, TextEncoder};

    let families = metrics.gather();
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    encoder.encode(&families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Submit 5 diverse background tasks to simulate a real agent workload.
async fn submit_background_tasks(api: &SupervisorApi) -> Result<(), Box<dyn std::error::Error>> {
    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: 1_000,
        max_ms: 5_000,
        factor: 2.0,
    };

    // ── Task 1: Heartbeat — echo every 5s (periodic, never fails) ───────────
    let heartbeat = TaskSpec::builder(
        "agent-heartbeat",
        TaskKind::Subprocess(SubprocessSpec {
            mode: SubprocessMode::Command {
                command: "echo".into(),
                args: vec!["heartbeat: alive".into()],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        }),
        3_000_u64,
    )
    .restart(RestartPolicy::periodic(5_000))
    .backoff(backoff.clone())
    .build()
    .unwrap();
    let id = api.submit(&heartbeat).await?;
    info!("[1/5] agent-heartbeat submitted: {}", id);

    // ── Task 2: System monitor — uptime every 15s ───────────────────────────
    let sysmon = TaskSpec::builder(
        "sys-monitor",
        TaskKind::Subprocess(SubprocessSpec {
            mode: SubprocessMode::Command {
                command: "uptime".into(),
                args: vec![],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        }),
        5_000_u64,
    )
    .restart(RestartPolicy::periodic(15_000))
    .backoff(backoff.clone())
    .build()
    .unwrap();
    let id = api.submit(&sysmon).await?;
    info!("[2/5] sys-monitor submitted: {}", id);

    // ── Task 3: Disk check — df every 30s ───────────────────────────────────
    let disk_check = TaskSpec::builder(
        "disk-check",
        TaskKind::Subprocess(SubprocessSpec {
            mode: SubprocessMode::Command {
                command: "df".into(),
                args: vec!["-h".into()],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        }),
        5_000_u64,
    )
    .restart(RestartPolicy::periodic(30_000))
    .backoff(backoff.clone())
    .build()
    .unwrap();
    let id = api.submit(&disk_check).await?;
    info!("[3/5] disk-check submitted: {}", id);

    // ── Task 4: One-shot date — runs once and completes ─────────────────────
    let oneshot = TaskSpec::builder(
        "oneshot-date",
        TaskKind::Subprocess(SubprocessSpec {
            mode: SubprocessMode::Command {
                command: "date".into(),
                args: vec!["+%Y-%m-%d %H:%M:%S".into()],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        }),
        3_000_u64,
    )
    .backoff(backoff.clone())
    .build()
    .unwrap();
    let id = api.submit(&oneshot).await?;
    info!("[4/5] oneshot-date submitted: {}", id);

    // ── Task 5: Flaky job — fails intentionally, retries on failure ─────────
    let flaky = TaskSpec::builder(
        "flaky-job",
        TaskKind::Subprocess(SubprocessSpec {
            mode: SubprocessMode::Command {
                command: "sh".into(),
                args: vec!["-c".into(), "echo 'attempt running...'; exit 1".into()],
            },
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        }),
        5_000_u64,
    )
    .restart(RestartPolicy::OnFailure)
    .backoff(BackoffPolicy {
        jitter: JitterPolicy::Full,
        first_ms: 2_000,
        max_ms: 10_000,
        factor: 2.0,
    })
    .admission(AdmissionPolicy::Replace)
    .build()
    .unwrap();
    let id = api.submit(&flaky).await?;
    info!("[5/5] flaky-job submitted: {}", id);

    info!("all 5 background tasks submitted");
    Ok(())
}
