//! # Podium-connected agent
//!
//! Podium desired spec -> HTTP Task API -> core -> subprocess runner -> this binary.
//! Discovery advertises that runner; Podium displays its status, runs and live output.
//! The demo child prints for 30 seconds and exits successfully, without a shell,
//! files, network connections or external services.
//!
//! Run from the SDK root:
//! `cargo run --locked -p solti --example agent_podium --features api-core-adapter,api-http,discover-http,exec-subprocess`.
//! See `examples/podium/README.md` next to this file for the complete walkthrough.
//! `--print-podium-spec` emits the exact Podium REST request for this executable:
//! an `exec.solti.io/v1` `Subprocess` workload bound to the schema fingerprint the agent advertises.
//! `--demo-task` is the child mode; it does not start an agent.

#[path = "support/http_agent.rs"]
mod http_agent;

use std::{env, io, io::Write, time::Duration};

use http_agent::{Defaults, ExampleResult, Settings};
use solti::{
    exec::subprocess::{SubprocessMode, SubprocessSpec},
    model::{AgentCapabilities, Flag, TaskEnv, Workload},
    runner::RunnerRouter,
};

const DEMO_MODE: &str = "--demo-task";

fn settings() -> ExampleResult<Settings> {
    Settings::from_env(Defaults {
        agent_id: "podium-example-agent",
        agent_name: "Podium example agent",
        control_plane: "http://127.0.0.1:8082",
        api_address: "127.0.0.1:8085",
        interval_ms: 2_000,
    })
}

fn executable() -> ExampleResult<String> {
    env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "example path must be UTF-8").into()
        })
}

/// The Podium create request: the spec envelope and its workload, bound to the schema
/// fingerprint of the first registered runner that declares the body's type.
/// Podium's placement evaluates that same runner.
fn podium_spec(
    settings: &Settings,
    capabilities: &AgentCapabilities,
) -> ExampleResult<serde_json::Value> {
    let workload = Workload::from_body(&SubprocessSpec::new(
        SubprocessMode::Command {
            command: executable()?,
            args: vec![DEMO_MODE.into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ))?;
    let fingerprint = capabilities
        .runners()
        .iter()
        .flat_map(|runner| runner.workload_types())
        .find(|declared| {
            declared.api_version() == workload.api_version() && declared.kind() == workload.kind()
        })
        .map(|declared| declared.fingerprint().clone())
        .ok_or_else(|| {
            io::Error::other(format!(
                "no registered runner declares {}/{}",
                workload.api_version(),
                workload.kind(),
            ))
        })?;
    Ok(serde_json::json!({
        "name": "SDK Podium demo",
        "slot": "podium-example",
        "workload": {
            "api_version": workload.api_version(),
            "kind": workload.kind(),
            "schema_fingerprint": fingerprint,
            "body": workload.body()
        },
        "timeout_ms": 60000,
        "restart_type": "never",
        "interval_ms": 0,
        "jitter": "none",
        "backoff_first_ms": 1000,
        "backoff_max_ms": 5000,
        "backoff_factor": 2,
        "targets": [&settings.agent_id],
        "target_labels": {},
        "runner_labels": {}
    }))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [mode] if mode == DEMO_MODE => return demo_task().await,
        [mode] if mode == "--print-podium-spec" => return print_podium_spec().await,
        [mode] if mode == "--help" || mode == "-h" => {
            println!(
                "agent_podium [--print-podium-spec | --demo-task]\n\
                 No arguments: serve an HTTP agent and register with Podium.\n\
                 Environment: SOLTI_CONTROL_PLANE, SOLTI_AGENT_ADDR, SOLTI_AGENT_ID,\n\
                 SOLTI_AGENT_NAME, SOLTI_AGENT_ADVERTISE_URL.\n\
                 Walkthrough: crates/solti/examples/podium/README.md"
            );
            return Ok(());
        }
        [] => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "use --help for supported arguments",
            )
            .into());
        }
    }
    let settings = settings()?;
    println!(
        "solti: Podium-connected agent\n\
         Podium -> discovery + desired spec -> HTTP API -> core -> subprocess runner\n\
         Demo command: {}\n\
         Demo arguments (JSON): [\"--demo-task\"]\n\
         Demo timeout: 60000ms; restart: never\n\
         Use --print-podium-spec for the exact create request and the README for Deploy/logs.",
        serde_json::to_string(&executable()?)?,
    );
    http_agent::run(settings).await
}

async fn print_podium_spec() -> ExampleResult {
    let settings = settings()?;
    let mut router = RunnerRouter::new();
    let runner = http_agent::register_runners(&mut router)?;
    let spec = podium_spec(&settings, &router.capabilities());
    // Registration starts runner-owned workers; stop them before the process exits.
    let finalizer = runner.shutdown(Duration::from_secs(5)).await;
    println!("{}", serde_json::to_string_pretty(&spec?)?);
    finalizer?;
    Ok(())
}

async fn demo_task() -> ExampleResult {
    writeln!(io::stderr(), "podium-demo stderr: started")?;
    for tick in 1..=30 {
        writeln!(io::stdout(), "podium-demo stdout tick={tick:02}/30")?;
        io::stdout().flush()?;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    writeln!(io::stderr(), "podium-demo stderr: finished successfully")?;
    Ok(())
}
