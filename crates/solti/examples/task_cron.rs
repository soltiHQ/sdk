//! Calendar scheduling through the cron → Embedded Task → core → Taskvisor path.
//!
//! Run with `cargo run -p solti --example task_cron --features cron,core`.
//! The example observes two real UTC calendar invocations, cancels the outer
//! resource, verifies that later ticks do not run, and joins core shutdown.

use solti::{
    core::SupervisorApi,
    cron::{CronSchedule, CronTask},
    model::{EmbeddedSpec, RestartPolicy, TaskManifest, TaskPhase, TaskSpec, TaskWorkload},
    runner::RunnerRouter,
    taskvisor::{TaskContext, TaskError, TaskFn},
};
use std::{io, time::Duration};
use tokio::{sync::mpsc, time::timeout};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!(
        "cron calendar → sequential Embedded task → core/Taskvisor → cancellation → joined shutdown"
    );
    let api = SupervisorApi::builder(RunnerRouter::new()).start().await?;
    let result = run(&api).await;
    let shutdown = api.shutdown().await;
    result?;
    shutdown?;
    println!("[shutdown] Core and its supervised cron task stopped.");
    Ok(())
}

async fn run(api: &SupervisorApi) -> ExampleResult {
    let (ticks, mut observed) = mpsc::channel(4);
    let job = TaskFn::arc(move |ctx: TaskContext| {
        let ticks = ticks.clone();
        async move {
            ctx.run_until_cancelled(ticks.send(()))
                .await?
                .map_err(TaskError::fatal_from)?;
            Ok(())
        }
    });
    let task = CronTask::new(
        CronSchedule::parse("* * * * * *", "UTC")?,
        job,
        Duration::from_secs(1),
    )?
    .into_task();
    // This 30-second timeout is the demonstration's outer lifetime.
    // The invocation timeout above bounds each job independently.
    let spec = TaskSpec::builder(
        "calendar-maintenance",
        TaskWorkload::Embedded(EmbeddedSpec::new("cron-example-v1")?),
        30_000_u64,
    )
    .restart(RestartPolicy::Never)
    .build()?;
    let manifest = TaskManifest::new("calendar-example", spec)?;
    let name = manifest.name().clone();
    api.create_embedded_task(manifest, task).await?;
    for number in 1..=2 {
        timeout(Duration::from_secs(5), observed.recv())
            .await?
            .ok_or_else(|| io::Error::other("cron task stopped before its next invocation"))?;
        println!("[invocation] Observed calendar job {number}.");
    }
    api.cancel_task(&name).await?;
    let terminal = api
        .get_task(&name)
        .ok_or_else(|| io::Error::other("missing retained calendar task"))?;
    if terminal.phase() != &TaskPhase::Canceled {
        return Err(
            io::Error::other(format!("unexpected terminal phase: {}", terminal.phase())).into(),
        );
    }
    if matches!(
        timeout(Duration::from_millis(1_100), observed.recv()).await,
        Ok(Some(()))
    ) {
        return Err(io::Error::other("calendar invoked its job after cancellation").into());
    }
    println!(
        "[cancel] Retained phase=Canceled; the next calendar occurrence did not invoke the job."
    );
    Ok(())
}
