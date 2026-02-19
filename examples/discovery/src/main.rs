use std::collections::HashMap;
use std::sync::Arc;

use axum::routing::get;
use tracing::info;

use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};
use tno_api::{HttpApi, SupervisorApiAdapter};
use tno_core::{BuildContext, RunnerRouter, SupervisorApi, TaskPolicy};
use tno_discover::{DiscoverConfig, DiscoveryTransport};
use tno_exec::subprocess::register_subprocess_runner;
use tno_model::{
    AdmissionStrategy, BackoffStrategy, CreateSpec, Flag, JitterStrategy, RestartStrategy,
    RunnerLabels, TaskEnv, TaskKind,
};
use tno_observe::{LoggerConfig, LoggerLevel, Subscriber, init_logger, timezone_sync};
use tno_prometheus::PrometheusMetrics;

const AGENT_HTTP_ADDR: &str = "0.0.0.0:8085";
const CONTROL_PLANE_ENDPOINT: &str = "http://localhost:8082";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Logger
    let cfg = LoggerConfig {
        level: LoggerLevel::new("info")?,
        ..Default::default()
    };
    init_logger(&cfg)?;
    info!("logger initialized");

    // 2) Prometheus metrics
    let metrics = PrometheusMetrics::new()?;
    let metrics_handle = Arc::new(metrics.clone());
    info!("prometheus metrics initialized");

    // 3) Router + subprocess runner
    let ctx = BuildContext::new(TaskEnv::default(), metrics_handle);
    let mut router = RunnerRouter::new().with_context(ctx);
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

    // 5) Internal tasks: timezone sync
    let (tz_task, tz_spec) = timezone_sync();
    let tz_policy = TaskPolicy::from_spec(&tz_spec);
    supervisor.submit_with_task(tz_task, &tz_policy).await?;
    info!("timezone sync task submitted");

    // 6) Discovery — periodic sync with control plane
    let discover_config = DiscoverConfig {
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
    let (sync_task, sync_spec) = tno_discover::sync(discover_config);
    let sync_policy = TaskPolicy::from_spec(&sync_spec);
    supervisor.submit_with_task(sync_task, &sync_policy).await?;
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
    use tno_prometheus::{Encoder, TextEncoder};

    let families = metrics.gather();
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    encoder.encode(&families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Submit 5 diverse background tasks to simulate a real agent workload.
async fn submit_background_tasks(api: &SupervisorApi) -> Result<(), Box<dyn std::error::Error>> {
    let backoff = BackoffStrategy {
        jitter: JitterStrategy::Equal,
        first_ms: 1_000,
        max_ms: 5_000,
        factor: 2.0,
    };

    // ── Task 1: Heartbeat — echo every 5s (periodic, never fails) ───────────
    let heartbeat = CreateSpec {
        slot: "agent-heartbeat".to_string(),
        kind: TaskKind::Subprocess {
            command: "echo".into(),
            args: vec!["heartbeat: alive".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 3_000,
        restart: RestartStrategy::periodic(5_000),
        backoff: backoff.clone(),
        admission: AdmissionStrategy::DropIfRunning,
        labels: RunnerLabels::default(),
    };
    let id = api.submit(&heartbeat).await?;
    info!("[1/5] agent-heartbeat submitted: {}", id);

    // ── Task 2: System monitor — uptime every 15s ───────────────────────────
    let sysmon = CreateSpec {
        slot: "sys-monitor".to_string(),
        kind: TaskKind::Subprocess {
            command: "uptime".into(),
            args: vec![],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::periodic(15_000),
        backoff: backoff.clone(),
        admission: AdmissionStrategy::DropIfRunning,
        labels: RunnerLabels::default(),
    };
    let id = api.submit(&sysmon).await?;
    info!("[2/5] sys-monitor submitted: {}", id);

    // ── Task 3: Disk check — df every 30s ───────────────────────────────────
    let disk_check = CreateSpec {
        slot: "disk-check".to_string(),
        kind: TaskKind::Subprocess {
            command: "df".into(),
            args: vec!["-h".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::periodic(30_000),
        backoff: backoff.clone(),
        admission: AdmissionStrategy::DropIfRunning,
        labels: RunnerLabels::default(),
    };
    let id = api.submit(&disk_check).await?;
    info!("[3/5] disk-check submitted: {}", id);

    // ── Task 4: One-shot date — runs once and completes ─────────────────────
    let oneshot = CreateSpec {
        slot: "oneshot-date".to_string(),
        kind: TaskKind::Subprocess {
            command: "date".into(),
            args: vec!["+%Y-%m-%d %H:%M:%S".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 3_000,
        restart: RestartStrategy::Never,
        backoff: backoff.clone(),
        admission: AdmissionStrategy::DropIfRunning,
        labels: RunnerLabels::default(),
    };
    let id = api.submit(&oneshot).await?;
    info!("[4/5] oneshot-date submitted: {}", id);

    // ── Task 5: Flaky job — fails intentionally, retries on failure ─────────
    let flaky = CreateSpec {
        slot: "flaky-job".to_string(),
        kind: TaskKind::Subprocess {
            command: "sh".into(),
            args: vec!["-c".into(), "echo 'attempt running...'; exit 1".into()],
            env: TaskEnv::default(),
            cwd: None,
            fail_on_non_zero: Flag::enabled(),
        },
        timeout_ms: 5_000,
        restart: RestartStrategy::OnFailure,
        backoff: BackoffStrategy {
            jitter: JitterStrategy::Full,
            first_ms: 2_000,
            max_ms: 10_000,
            factor: 2.0,
        },
        admission: AdmissionStrategy::Replace,
        labels: RunnerLabels::default(),
    };
    let id = api.submit(&flaky).await?;
    info!("[5/5] flaky-job submitted: {}", id);

    info!("all 5 background tasks submitted");
    Ok(())
}
