//! # Subprocess task: run one routed workload
//!
//! This example runs the example binary itself as a subprocess.
//! It does not invoke a shell or another executable.
//!
//! It shows the complete local path:
//! - `RunnerRouter` selects the subprocess runner;
//! - `SupervisorApi` commits and reconciles the task;
//! - the child publishes live stdout and stderr;
//! - core retains the terminal task and its run history;
//! - shutdown waits for SDK-owned workers.
//!
//! Live output is not replayed.
//! The child waits for the parent to subscribe before it writes output.
//! Timeouts below are failure bounds, not synchronization delays.
//!
//! ```text
//! Subprocess manifest
//!      │ create_task
//!      ▼
//! SupervisorApi ──► committed desired state ──► reconciliation
//!                                                   │ GVK routing
//!                                                   ▼
//!                                            RunnerRouter
//!                                                   ▼
//!                                         subprocess runner
//!                                                   │ spawn this binary
//!                                                   ▼
//!                                                child
//!                                         stdout + stderr
//!                     OutputSubscription ◄──────────┤
//!                     task watch         ◄──────────┤
//!                     run history        ◄──────────┘
//! ```
//!
//! Run with `cargo run -p solti --example task_subprocess --features core,exec-subprocess`.

use std::{env, io, time::Duration};

use solti::{
    core::{OutputSubscription, SupervisorApi, TaskWatchSubscription},
    exec::subprocess::register_subprocess_runner,
    model::{
        ConditionStatus, Flag, OutputEvent, RestartPolicy, StreamKind, SubprocessMode,
        SubprocessSpec, Task, TaskEnv, TaskFilter, TaskId, TaskManifest, TaskRunQuery, TaskSpec,
        TaskWorkload,
    },
    runner::RunnerRouter,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_stream::StreamExt;

const CHILD_MODE: &str = "--example-child";
const WAIT_BOUND: Duration = Duration::from_secs(10);

type ExampleError = Box<dyn std::error::Error>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), ExampleError> {
    let mut args = env::args();
    let _program = args.next();
    if args.next().as_deref() == Some(CHILD_MODE) {
        let address = args
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent address"))?;
        return child(&address).await;
    }

    println!(
        r#"
solti: one supervised subprocess

  manifest ──► core ──► router ──► subprocess runner ──► child process
                 ├──► task watch                           ├──► stdout
                 ├──► live output ◄────────────────────────┤
                 └──► run history                          └──► stderr
"#
    );
    println!("[purpose] Execute one routed workload and observe its complete resource lifecycle.");

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let child_address = listener.local_addr()?;
    let command = env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "example path is not UTF-8"))?;

    let mut router = RunnerRouter::new();
    let subprocess_runner = register_subprocess_runner(&mut router, "default")?;
    println!("[runner] Registered the built-in Subprocess GVK as runner=default.");

    let supervisor = SupervisorApi::builder(router).start().await?;
    let run_result = run(&supervisor, listener, child_address.to_string(), command).await;
    let shutdown_result = supervisor.shutdown().await;
    let finalizer_result = subprocess_runner.shutdown(Duration::from_secs(5)).await;

    run_result?;
    shutdown_result?;
    finalizer_result?;
    println!("[shutdown] Supervisor and SDK-owned workers stopped.");
    println!(
        "\nResult: one routed subprocess completed with live output, terminal state, and retained history."
    );
    Ok(())
}

async fn child(address: &str) -> Result<(), ExampleError> {
    let mut parent = TcpStream::connect(address).await?;
    let mut release = [0_u8; 1];
    parent.read_exact(&mut release).await?;

    println!("child stdout");
    eprintln!("child stderr");
    Ok(())
}

async fn run(
    supervisor: &SupervisorApi,
    listener: TcpListener,
    child_address: String,
    command: String,
) -> Result<(), ExampleError> {
    let mut watch = supervisor.watch_tasks(&TaskFilter::new(), None)?;
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command,
            args: vec![CHILD_MODE.into(), child_address],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("example", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("subprocess-example", spec)?;
    let task_name = manifest.name().clone();

    let committed = supervisor.create_task(manifest).await?;
    println!(
        "committed {} generation {}",
        committed.name(),
        committed.metadata().generation()
    );

    let reconciled = wait_for_reconciliation(&mut watch, &task_name).await?;
    let condition = reconciled.status().reconciled();
    println!(
        "Reconciled={:?} reason={}",
        condition.status(),
        condition.reason()
    );

    let (mut child_control, _) = timeout(WAIT_BOUND, listener.accept()).await??;
    let output = supervisor
        .subscribe_output(&task_name)
        .ok_or_else(|| io::Error::other("live output is unavailable"))?;
    child_control.write_all(&[1]).await?;
    drop(child_control);

    print_child_output(output).await?;

    let terminal = wait_for_terminal_phase(&mut watch, &task_name).await?;
    println!("terminal phase={}", terminal.status().phase());

    let runs = supervisor
        .query_task_runs(&task_name, &TaskRunQuery::new())?
        .ok_or_else(|| io::Error::other("task disappeared before run history was read"))?;
    let run = runs
        .items
        .last()
        .ok_or_else(|| io::Error::other("run history is empty"))?;
    println!(
        "run generation={} attempt={} phase={}",
        run.generation(),
        run.attempt(),
        run.phase(),
    );
    Ok(())
}

async fn wait_for_reconciliation(
    watch: &mut TaskWatchSubscription,
    task_name: &TaskId,
) -> Result<Task, ExampleError> {
    loop {
        let task = next_task(watch, task_name).await?;
        let condition = task.status().reconciled();
        match condition.status() {
            ConditionStatus::Unknown => {}
            ConditionStatus::True => return Ok(task),
            ConditionStatus::False => {
                return Err(io::Error::other(format!(
                    "reconciliation failed: {}: {}",
                    condition.reason(),
                    condition.message()
                ))
                .into());
            }
            _ => {}
        }
    }
}

async fn wait_for_terminal_phase(
    watch: &mut TaskWatchSubscription,
    task_name: &TaskId,
) -> Result<Task, ExampleError> {
    loop {
        let task = next_task(watch, task_name).await?;
        if task.status().phase().is_terminal() {
            return Ok(task);
        }
    }
}

async fn next_task(
    watch: &mut TaskWatchSubscription,
    task_name: &TaskId,
) -> Result<Task, ExampleError> {
    loop {
        let event = timeout(WAIT_BOUND, watch.next())
            .await?
            .ok_or_else(|| io::Error::other("task watch closed"))??;
        let task = event.into_object();
        if task.name() == task_name {
            return Ok(task);
        }
    }
}

async fn print_child_output(mut output: OutputSubscription) -> Result<(), ExampleError> {
    let mut saw_stdout = false;
    let mut saw_stderr = false;

    while !saw_stdout || !saw_stderr {
        let event = timeout(WAIT_BOUND, output.next())
            .await?
            .ok_or_else(|| io::Error::other("live output closed"))?;
        match event {
            OutputEvent::Chunk(chunk) => {
                let line = String::from_utf8_lossy(&chunk.line);
                println!("live {:?}: {line}", chunk.stream);
                saw_stdout |= chunk.stream == StreamKind::Stdout;
                saw_stderr |= chunk.stream == StreamKind::Stderr;
            }
            OutputEvent::Lagged {
                skipped,
                skipped_bytes,
            } => {
                return Err(io::Error::other(format!(
                    "live output lost {skipped} events ({skipped_bytes} bytes)"
                ))
                .into());
            }
            OutputEvent::RunStarted { .. } | OutputEvent::RunFinished { .. } => {}
            _ => {}
        }
    }
    Ok(())
}
