//! # Chain task: route four subprocess steps
//!
//! This example runs the example binary itself as every subprocess step.
//! It does not invoke a shell, another executable, or an external service.
//!
//! The outcome-directed path is:
//!
//! ```text
//! task1 succeeds ──► task2 succeeds ──► task3 fails ──► task4 recovers
//!                                                              ▼
//!                                                     outer Task succeeds
//! ```
//!
//! It shows the complete local path:
//! - the subprocess runner is snapshotted into the Chain allowlist;
//! - `RunnerRouter` selects the Chain runner for the outer Task;
//! - every step is routed to the subprocess runner;
//! - all step output is published through one outer live stream;
//! - core retains one successful outer run;
//! - shutdown waits for SDK-owned workers.
//!
//! Live output is not replayed.
//! Each child waits for the parent to subscribe before it writes output.
//! Timeouts below are failure bounds, not delays.
//!
//! Run with
//! `cargo run -p solti --example task_chain --features chain,core,exec-subprocess`.

use std::{env, io, time::Duration};

use solti::{
    chain::{ChainSpec, ChainStep, FailureMode, register_chain_runner},
    core::{OutputSubscription, SupervisorApi, TaskWatchSubscription},
    exec::subprocess::register_subprocess_runner,
    model::{
        ConditionStatus, Flag, OutputEvent, RestartPolicy, SubprocessMode, SubprocessSpec, Task,
        TaskEnv, TaskFilter, TaskId, TaskManifest, TaskPhase, TaskSpec, TaskWorkload,
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
const FAIL: &str = "fail";
const STEP_COUNT: usize = 4;
const WAIT_BOUND: Duration = Duration::from_secs(10);

type ExampleError = Box<dyn std::error::Error>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), ExampleError> {
    let mut args = env::args();
    let _program = args.next();
    if args.next().as_deref() == Some(CHILD_MODE) {
        let address = required_arg(args.next(), "parent address")?;
        let step = required_arg(args.next(), "step name")?;
        let outcome = required_arg(args.next(), "step outcome")?;
        return child(&address, &step, outcome == FAIL).await;
    }

    println!(
        r#"
solti: one supervised conditional chain

  outer Task ──► Chain ──► task1 ──success─► task2 ──success─► task3
                                                                  │ failure
                                                                  ▼
                                                               task4
                                                                  │ recover
                                                                  ▼
                                                         outer Succeeded
"#
    );
    println!(
        "[purpose] Execute four routed subprocess workloads and recover the outer Task after task3 fails."
    );

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let child_address = listener.local_addr()?.to_string();
    let command = env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "example path is not UTF-8"))?;

    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "subprocess")?;
    register_chain_runner(&mut router, "chain")?;
    println!("[runner] Registered Subprocess first, then snapshotted it into the Chain allowlist.");

    let supervisor = SupervisorApi::builder(router).start().await?;
    let run_result = run(&supervisor, listener, child_address, command).await;
    let shutdown_result = supervisor.shutdown().await;

    run_result?;
    shutdown_result?;
    println!("[shutdown] Supervisor and SDK-owned workers stopped.");
    println!(
        "\nResult: task1 and task2 succeeded, task3 failed, task4 recovered, and the outer Task completed successfully with live output and retained history."
    );
    Ok(())
}

fn required_arg(value: Option<String>, name: &str) -> Result<String, ExampleError> {
    value.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("missing {name}")).into()
    })
}

async fn child(address: &str, step: &str, fail: bool) -> Result<(), ExampleError> {
    let mut parent = TcpStream::connect(address).await?;
    let mut release = [0_u8; 1];
    parent.read_exact(&mut release).await?;

    println!("{step} stdout");
    if fail {
        eprintln!("{step} intentional failure");
        return Err(io::Error::other(format!("{step} failed as requested")).into());
    }
    Ok(())
}

async fn run(
    supervisor: &SupervisorApi,
    listener: TcpListener,
    child_address: String,
    command: String,
) -> Result<(), ExampleError> {
    let mut watch = supervisor.watch_tasks(&TaskFilter::new(), None)?;
    let chain = ChainSpec::new(
        "task1",
        vec![
            ChainStep::new(
                "task1",
                subprocess(&command, &child_address, "task1", false),
            )?
            .with_on_success("task2")?,
            ChainStep::new(
                "task2",
                subprocess(&command, &child_address, "task2", false),
            )?
            .with_on_success("task3")?,
            ChainStep::new("task3", subprocess(&command, &child_address, "task3", true))?
                .with_on_failure("task4", FailureMode::Recover)?,
            ChainStep::new(
                "task4",
                subprocess(&command, &child_address, "task4", false),
            )?,
        ],
    )?;
    let spec = TaskSpec::builder("example", chain.into_workload()?, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("chain-example", spec)?;
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

    let output = supervisor
        .subscribe_output(&task_name)
        .ok_or_else(|| io::Error::other("live output is unavailable"))?;
    let (output_result, release_result) =
        tokio::join!(print_chain_output(output), release_children(listener));
    output_result?;
    release_result?;

    let terminal = wait_for_terminal_phase(&mut watch, &task_name).await?;
    println!("terminal phase={}", terminal.status().phase());
    if terminal.status().phase() != TaskPhase::Succeeded {
        return Err(io::Error::other(format!(
            "outer Task did not recover: terminal phase is {}",
            terminal.status().phase()
        ))
        .into());
    }

    let runs = supervisor.list_task_runs(&task_name);
    let run = runs
        .last()
        .ok_or_else(|| io::Error::other("run history is empty"))?;
    println!(
        "run generation={} attempt={} phase={}",
        run.generation(),
        run.attempt(),
        run.phase(),
    );
    if runs.len() != 1 || run.phase() != TaskPhase::Succeeded {
        return Err(io::Error::other("expected one successful outer run in history").into());
    }
    Ok(())
}

fn subprocess(command: &str, address: &str, step: &str, fail: bool) -> TaskWorkload {
    TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: command.into(),
            args: vec![
                CHILD_MODE.into(),
                address.into(),
                step.into(),
                if fail { FAIL.into() } else { "succeed".into() },
            ],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ))
}

async fn release_children(listener: TcpListener) -> Result<(), ExampleError> {
    for ordinal in 1..=STEP_COUNT {
        let (mut child_control, _) = timeout(WAIT_BOUND, listener.accept()).await??;
        child_control.write_all(&[1]).await?;
        println!("[control] released child step {ordinal}/{STEP_COUNT}");
    }
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

async fn print_chain_output(mut output: OutputSubscription) -> Result<(), ExampleError> {
    let mut saw_steps = [false; STEP_COUNT];
    let mut saw_intentional_failure = false;

    loop {
        let event = timeout(WAIT_BOUND, output.next())
            .await?
            .ok_or_else(|| io::Error::other("live output closed before RunFinished"))?;
        match event {
            OutputEvent::Chunk(chunk) => {
                let line = String::from_utf8_lossy(&chunk.line);
                println!(
                    "live {:?} generation={} attempt={} seq={}: {line}",
                    chunk.stream, chunk.generation, chunk.attempt, chunk.seq
                );
                for (index, saw_step) in saw_steps.iter_mut().enumerate() {
                    *saw_step |= line == format!("task{} stdout", index + 1);
                }
                saw_intentional_failure |= line == "task3 intentional failure";
            }
            OutputEvent::RunStarted {
                generation,
                attempt,
                ..
            } => {
                println!("live RunStarted generation={generation} attempt={attempt}");
            }
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                ..
            } => {
                println!(
                    "live RunFinished generation={generation} attempt={attempt} exitCode={exit_code:?}"
                );
                break;
            }
            OutputEvent::Lagged { skipped } => {
                return Err(io::Error::other(format!("live output lost {skipped} events")).into());
            }
            _ => {}
        }
    }

    if !saw_steps.into_iter().all(|seen| seen) || !saw_intentional_failure {
        return Err(io::Error::other(
            "live output did not include all four steps and task3 failure",
        )
        .into());
    }
    Ok(())
}
