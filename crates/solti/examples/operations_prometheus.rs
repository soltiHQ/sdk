//! # Prometheus operations: scrape a real agent runtime
//!
//! One application-owned Prometheus registry receives metrics from runner and Taskvisor adapters.
//! A core state collector reads the same state retained by the supervisor.
//! The registry is served by a supervised Embedded task.
//!
//! This example shows:
//!
//! - one shared registry;
//! - runner metrics injected through `BuildContext`;
//! - a Taskvisor event subscriber installed through core;
//! - a collector backed by the supervisor's `TaskState`;
//! - a supervised `GET /metrics` endpoint;
//! - one successful subprocess and one spawn failure;
//! - real Prometheus samples produced by that work.
//!
//! ```text
//! subprocess runner ──► PrometheusRunnerMetrics ───────┐
//! Taskvisor events ──► PrometheusTaskvisorSubscriber ──┤
//! core TaskState ─────► PrometheusCoreStateCollector ──┤
//! build labels ───────► register_build_info ───────────┤
//!                                                      ▼
//!                                               shared Registry
//!                                                      │
//!                                      supervised Embedded task
//!                                                      │
//!                                                      ▼
//!                                                GET /metrics
//! ```
//!
//! Run with `cargo run -p solti --example operations_prometheus --features core,exec-subprocess,prometheus,prometheus-server,prometheus-state`.

use std::{env, io, sync::Arc, time::Duration};

use solti::{
    core::{SupervisorApi, TaskWatchSubscription},
    exec::subprocess::register_subprocess_runner,
    model::{
        Flag, RestartPolicy, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskManifest,
        TaskSpec, TaskWorkload,
    },
    prometheus::{
        PrometheusCoreStateCollector, PrometheusRunnerMetrics, PrometheusTaskvisorSubscriber,
        Registry, register_build_info, server,
    },
    runner::{BuildContext, MetricsHandle, RunnerRouter},
    taskvisor::Subscribe,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout},
};
use tokio_stream::StreamExt;

const CHILD_MODE: &str = "--operations-prometheus-child";
const METRICS_ADDRESS: &str = "127.0.0.1:9090";
const WAIT_BOUND: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_millis(25);

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    if env::args().nth(1).as_deref() == Some(CHILD_MODE) {
        println!("metrics example child completed");
        return Ok(());
    }

    println!(
        r#"
solti: supervised Prometheus endpoint

  supervised subprocesses
      ├──► runner callbacks ──► runner metrics ────┐
      ├──► Taskvisor events ──► lifecycle metrics ─┼──► Registry
      └──► core TaskState ─────► phase collector ──┘       │
                                                           └──► GET /metrics
"#
    );
    println!(
        "[purpose] Run real work, expose its operational metrics, and inspect one Prometheus scrape."
    );

    let registry = Arc::new(Registry::new());
    register_build_info(
        &registry,
        &[
            ("component", "operations-prometheus-example"),
            ("version", env!("CARGO_PKG_VERSION")),
        ],
    )?;

    let runner_metrics: MetricsHandle = Arc::new(PrometheusRunnerMetrics::new(&registry)?);
    let context = BuildContext::default().with_metrics(runner_metrics);
    let mut router = RunnerRouter::new().with_context(context);
    register_subprocess_runner(&mut router, "default")?;

    let taskvisor_metrics: Arc<dyn Subscribe> =
        Arc::new(PrometheusTaskvisorSubscriber::new(&registry)?);
    let supervisor = SupervisorApi::builder(router)
        .with_subscribers(vec![taskvisor_metrics])
        .start()
        .await?;
    registry.register(Box::new(PrometheusCoreStateCollector::new(
        supervisor.state(),
    )?))?;
    println!("[registry] Registered build, runner, Taskvisor, and core-state collectors.");

    let metrics_address =
        env::var("SOLTI_METRICS_ADDR").unwrap_or_else(|_| METRICS_ADDRESS.to_string());
    let (metrics_manifest, metrics_task) = server(
        Arc::clone(&registry),
        metrics_address.clone(),
        "operations-prometheus-v1",
    )?;
    supervisor
        .create_embedded_task(metrics_manifest, metrics_task)
        .await?;
    println!("[endpoint] Starting http://{metrics_address}/metrics as an Embedded task.");

    let mut watch = supervisor.watch_tasks(&Default::default(), None)?;
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: current_executable()?,
            args: vec![CHILD_MODE.into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("metrics-example", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("measured-subprocess", spec)?;
    let task_name = manifest.name().clone();
    supervisor.create_task(manifest).await?;

    let terminal = wait_for_terminal(&mut watch, &task_name).await?;
    println!(
        "[task/success] name={task_name}, phase={}.",
        terminal.status().phase()
    );

    let missing_command = format!("{}.missing", current_executable()?);
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: missing_command,
            args: Vec::new(),
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("metrics-failure", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("measured-spawn-failure", spec)?;
    let failed_name = manifest.name().clone();
    supervisor.create_task(manifest).await?;
    let failed = wait_for_terminal(&mut watch, &failed_name).await?;
    println!(
        "[task/failure] name={failed_name}, phase={}.",
        failed.status().phase()
    );

    let exposition = scrape_metrics(&metrics_address).await?;
    println!("[scrape] Samples produced by the running metrics task and both subprocess tasks:");
    print_nonzero_samples(&exposition, "solti_build_info");
    print_nonzero_samples(&exposition, "solti_core_tasks_by_phase");
    print_nonzero_samples(&exposition, "solti_runner_errors_total");
    print_nonzero_samples(&exposition, "solti_taskvisor_attempts_in_flight");
    print_nonzero_samples(
        &exposition,
        "solti_taskvisor_controller_submitted_events_total",
    );
    print_nonzero_samples(&exposition, "solti_taskvisor_task_final_outcomes_total");

    println!("[meaning] running=1 and attempts_in_flight=1 are the live /metrics task.");
    println!("[meaning] succeeded=1 and outcome_completed=1 are the successful subprocess.");
    println!("[meaning] failed=1, spawn_failed=1, and outcome_fatal=1 are the missing executable.");
    println!("[meaning] submitted_events=3 counts the endpoint and both subprocess tasks.");

    println!("\n[curl] curl --silent http://{metrics_address}/metrics | grep '^solti_'");
    println!("[wait] The endpoint is active. Press Ctrl-C to stop the supervisor.");
    tokio::signal::ctrl_c().await?;

    println!("[shutdown] Stopping the metrics endpoint and remaining supervised work.");
    supervisor.shutdown().await?;
    println!("Result: real runtime metrics were available through a supervised /metrics endpoint.");
    Ok(())
}

fn current_executable() -> ExampleResult<String> {
    env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "example path is not UTF-8").into())
}

async fn wait_for_terminal(
    watch: &mut TaskWatchSubscription,
    name: &TaskId,
) -> Result<Task, Box<dyn std::error::Error>> {
    loop {
        let event = timeout(WAIT_BOUND, watch.next())
            .await?
            .ok_or_else(|| io::Error::other("task watch closed"))??;
        let task = event.into_object();
        if task.name() == name && task.status().phase().is_terminal() {
            return Ok(task);
        }
    }
}

async fn scrape_metrics(address: &str) -> ExampleResult<String> {
    let response = timeout(WAIT_BOUND, async {
        loop {
            match scrape_once(address).await {
                Ok(response) => return Ok::<_, io::Error>(response),
                Err(_) => sleep(RETRY_DELAY).await,
            }
        }
    })
    .await??;

    let response = String::from_utf8(response)?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| io::Error::other("metrics endpoint returned an invalid HTTP response"))?;
    let status = headers
        .lines()
        .next()
        .ok_or_else(|| io::Error::other("metrics endpoint returned no HTTP status"))?;
    if !status.contains(" 200 ") {
        return Err(io::Error::other(format!("metrics endpoint returned {status}")).into());
    }
    Ok(body.to_string())
}

async fn scrape_once(address: &str) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(address).await?;
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(response)
}

fn print_nonzero_samples(exposition: &str, name: &str) {
    let mut found = false;
    for line in exposition.lines().filter(|line| {
        line.strip_prefix(name)
            .is_some_and(|suffix| suffix.starts_with('{') || suffix.starts_with(' '))
            && !line.ends_with(" 0")
    }) {
        println!("         {line}");
        found = true;
    }
    assert!(found, "non-zero metric {name} is missing");
}
