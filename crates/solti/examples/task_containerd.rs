//! # Containerd task: supervise one native container workload
//!
//! This example connects the native containerd 2.x adapter to `solti-core`.
//! Core owns desired state while the container runner owns attempt resources.
//!
//! This example shows:
//!
//! - explicit containerd endpoint and plugin configuration;
//! - hardened OCI process policy;
//! - GVK registration through `RunnerRouter`;
//! - desired-state commit and background reconciliation;
//! - one real container attempt;
//! - terminal task state and retained run history.
//!
//! ```text
//! Container manifest ──► SupervisorApi ──► RunnerRouter
//!                                                ▼
//!                                        ContainerRunner
//!                                                ▼
//!                                       ContainerdEngine
//!                                 pull ─ snapshot ─ task ─ cleanup
//!                                                ▼
//!                                      terminal Task + TaskRun
//! ```
//!
//! The example requires Linux and an accessible containerd 2.x daemon.
//! It does not start or discover the daemon.
//!
//! Run with `cargo run -p solti --example task_containerd --features core,exec-containerd`.

use std::{io, sync::Arc, time::Duration};

use solti::{
    core::{SupervisorApi, TaskWatchSubscription},
    exec::{
        container::{
            ContainerProcessPolicy, ContainerRunnerConfig,
            containerd::{ContainerNetwork, ContainerdConfig, ContainerdEngine},
            register_container_runner_with_config,
        },
        isolation::SeccompPolicy,
    },
    model::{
        ContainerSpec, RestartPolicy, Task, TaskEnv, TaskId, TaskManifest, TaskRunQuery, TaskSpec,
        TaskWorkload,
    },
    runner::RunnerRouter,
};
use tokio::time::timeout;
use tokio_stream::StreamExt;

const WAIT_BOUND: Duration = Duration::from_secs(180);

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn network_setting() -> ExampleResult<ContainerNetwork> {
    match setting("SOLTI_CONTAINER_NETWORK", "none").as_str() {
        "none" => Ok(ContainerNetwork::None),
        "host" => Ok(ContainerNetwork::Host),
        value => {
            Err(format!("SOLTI_CONTAINER_NETWORK must be `none` or `host`, got {value:?}").into())
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!(
        r#"
solti: supervised containerd workload

  Container desired state ──► core ──► container runner ──► containerd 2.x
                                  ├──► task watch                  ├──► OCI runtime
                                  └──► run history ◄───────────────┴──► cleanup
"#
    );
    println!(
        "[purpose] Execute a Container resource through the full model, router, core, and native engine path."
    );

    if !cfg!(target_os = "linux") {
        println!(
            "[platform] Native containerd attempts require Linux; current OS is {}.",
            std::env::consts::OS,
        );
        println!("\nResult: prerequisites were explained; no daemon was contacted.");
        return Ok(());
    }

    let socket = setting("SOLTI_CONTAINERD_SOCKET", "/run/containerd/containerd.sock");
    let namespace = setting("SOLTI_CONTAINERD_NAMESPACE", "solti");
    let snapshotter = setting("SOLTI_CONTAINERD_SNAPSHOTTER", "overlayfs");
    let runtime = setting("SOLTI_CONTAINERD_RUNTIME", "io.containerd.runc.v2");
    let image = setting("SOLTI_CONTAINER_IMAGE", "docker.io/library/alpine:3.21");
    let network = network_setting()?;
    println!(
        "[engine] socket={socket}, namespace={namespace}, snapshotter={snapshotter}, runtime={runtime}."
    );
    println!("[workload] image={image}, network={network:?}.");

    let engine = Arc::new(
        ContainerdEngine::connect(
            ContainerdConfig::new(socket, namespace, snapshotter, runtime).with_network(network),
        )
        .await?,
    );
    let info = engine.probe().await?;
    println!(
        "[probe] Connected to engine={} version={}.",
        info.name(),
        info.version(),
    );

    let process_policy = ContainerProcessPolicy::new()
        .with_capabilities([])
        .with_no_new_privileges(true)
        .with_umask(0o077)
        .with_seccomp(SeccompPolicy::DenyHostControl);
    let config = ContainerRunnerConfig::new().with_process_policy(process_policy);
    println!("[policy] capabilities=[], noNewPrivileges=true, umask=077, seccomp=DenyHostControl.");

    let mut router = RunnerRouter::new();
    register_container_runner_with_config(&mut router, "containerd", engine.clone(), config)?;
    let supervisor = SupervisorApi::builder(router).start().await?;
    let mut watch = supervisor.watch_tasks(&Default::default(), None)?;

    let mut env = TaskEnv::new();
    env.push("SOLTI_EXAMPLE", "umbrella-container");
    let workload = TaskWorkload::Container(ContainerSpec::new(
        image,
        Some(vec!["/bin/sh".into()]),
        vec![
            "-c".into(),
            "test \"$SOLTI_EXAMPLE\" = umbrella-container".into(),
        ],
        env,
    ));
    let spec = TaskSpec::builder("containers", workload, 120_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("container-example", spec)?;
    let task_name = manifest.name().clone();

    let committed = supervisor.create_task(manifest).await?;
    println!(
        "[core] Committed task={} generation={}.",
        committed.name(),
        committed.metadata().generation(),
    );

    let terminal = wait_for_terminal(&mut watch, &task_name).await?;
    let runs = supervisor
        .query_task_runs(&task_name, &TaskRunQuery::new())?
        .ok_or_else(|| io::Error::other("task disappeared before run history was read"))?;
    let run = runs
        .items
        .last()
        .ok_or_else(|| io::Error::other("container run history is empty"))?;
    println!(
        "[result] taskPhase={}, runPhase={}, attempt={}.",
        terminal.status().phase(),
        run.phase(),
        run.attempt(),
    );

    supervisor.shutdown().await?;
    engine.shutdown().await?;
    println!("[shutdown] Supervisor and containerd cleanup worker stopped.");
    println!(
        "\nResult: core retained the completed resource after containerd removed its attempt-owned resources."
    );
    Ok(())
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
