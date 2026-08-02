//! # Observe operations: logging and supervised maintenance
//!
//! A binary installs process-wide logging before it starts the runtime.
//! The local-time offset refresh is then managed as an embedded core task.
//!
//! This example shows:
//!
//! - deterministic text logger configuration;
//! - structured events from the application and SDK;
//! - the supervised timezone refresh task;
//! - one routed subprocess workload;
//! - shutdown of both routed and embedded tasks.
//!
//! ```text
//! LoggerConfig ──► init_logger ──► global tracing subscriber
//!                                      ▲
//!                                      │ events
//! SupervisorApi ──┬──► timezone sync embedded task
//!                 └──► subprocess runner ──► child process
//! ```
//!
//! Run with `cargo run -p solti --example operations_observe --features core,exec-subprocess,observe-timezone-sync`.

use std::{env, io, time::Duration};

use solti::{
    core::{SupervisorApi, TaskWatchSubscription},
    exec::subprocess::register_subprocess_runner,
    model::{
        Flag, RestartPolicy, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskManifest,
        TaskSpec, TaskWorkload,
    },
    observe::{
        LoggerConfig, LoggerFormat, LoggerLevel, LoggerTimeZone, init_logger, timezone_sync,
    },
    runner::RunnerRouter,
};
use tokio::time::timeout;
use tokio_stream::StreamExt;

const CHILD_MODE: &str = "--operations-observe-child";
const WAIT_BOUND: Duration = Duration::from_secs(10);

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    if env::args().nth(1).as_deref() == Some(CHILD_MODE) {
        println!("observe operations child completed");
        return Ok(());
    }

    println!(
        r#"
solti: logging and supervised maintenance

  application + core + Taskvisor ──► tracing events ──► text logger
                 ├──► periodic local-offset refresh
                 └──► one routed subprocess
"#
    );
    println!(
        "[purpose] Install logging once and place recurring application maintenance under the same supervisor."
    );

    init_logger(&LoggerConfig {
        format: LoggerFormat::Text,
        level: LoggerLevel::new("info")?,
        timezone: LoggerTimeZone::Local,
        with_targets: true,
        use_color: false,
    })?;
    tracing::info!(target: "example::operations", "logger installed");

    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;
    let supervisor = SupervisorApi::builder(router).start().await?;

    let (timezone_manifest, timezone_task) = timezone_sync();
    let timezone_name = timezone_manifest.name().clone();
    supervisor
        .create_embedded_task(timezone_manifest, timezone_task)
        .await?;
    tracing::info!(
        target: "example::operations",
        task = %timezone_name,
        "timezone refresh is supervised"
    );

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
    let spec = TaskSpec::builder("observable-example", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("observable-subprocess", spec)?;
    let task_name = manifest.name().clone();
    supervisor.create_task(manifest).await?;

    let terminal = wait_for_terminal(&mut watch, &task_name).await?;
    tracing::info!(
        target: "example::operations",
        task = %task_name,
        phase = %terminal.status().phase(),
        "routed task reached terminal state"
    );

    supervisor.shutdown().await?;
    tracing::info!(target: "example::operations", "supervisor stopped");
    println!(
        "\nResult: routed work and recurring in-process maintenance emitted through one validated logger."
    );
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
