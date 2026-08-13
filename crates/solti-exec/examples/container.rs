//! # Container
//!
//! A `Container` workload becomes one reusable Taskvisor task.
//! Each attempt creates and removes containerd-owned runtime resources.
//!
//! This example shows:
//!
//! - explicit connection to containerd 2.x;
//! - native engine registration and GVK routing;
//! - image, command, arguments, and environment resolution;
//! - an isolated or host network namespace;
//! - an OCI process-hardening policy;
//! - stdout and stderr publication;
//! - one complete container attempt and cleanup.
//!
//! The example requires Linux and an accessible containerd 2.x daemon.
//! The configured snapshotter and OCI runtime must be available.
//! The image must be cached or reachable through containerd.
//!
//! Run with `cargo run -p solti-exec --example container --features containerd`.

use std::sync::{Arc, Mutex};

use solti_exec::container::containerd::{ContainerNetwork, ContainerdConfig, ContainerdEngine};
use solti_exec::container::{
    ContainerEngine, ContainerProcessPolicy, ContainerRunnerConfig,
    register_container_runner_with_config,
};
use solti_exec::isolation::SeccompPolicy;
use solti_model::{ContainerSpec, OutputEvent, Task, TaskEnv, TaskId, TaskSpec, TaskWorkload};
use solti_runner::{OutputPublisher, OutputPublisherHandle, OutputSink, RunnerRouter};
use taskvisor::TaskContext;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-exec: one native container attempt

  explicit containerd 2.x socket ──► ContainerdEngine::connect()
                                                │ probe endpoint + plugins
                                                ▼
  Task { workload: Container } ──► RunnerRouter ──► ContainerRunner
      │ image + command + args + env                     │ build
      │                                                  ▼
      │                                          reusable TaskRef
      │                                                  │ spawn attempt 1
      └──────────────────────────────────────────────────┤
                                                         ▼
                                               native containerd 2.x
                                                  ├──► pull + unpack image
                                                  ├──► snapshot
                                                  ├──► container + task
                                                  ├──► stdout/stderr ──► OutputPublisher
                                                  ├──► start + wait
                                                  └──► cleanup owned resources

  Network::None creates an empty network namespace without CNI.
  Network::Host shares the host network namespace explicitly.
"#;

#[derive(Default)]
struct RecordingOutput {
    events: Arc<Mutex<Vec<OutputEvent>>>,
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(&self, task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        println!(
            "[output] Opened a sink for task={task_name}, generation={generation}, attempt={attempt}."
        );
        let events = Arc::clone(&self.events);
        Some(OutputSink::new(generation, attempt, move |event| {
            events
                .lock()
                .expect("output recorder lock must not be poisoned")
                .push(event);
        }))
    }
}

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
    println!("{FLOW}");
    println!(
        "[purpose] Execute one real Container workload through the native containerd 2.x adapter."
    );

    if !cfg!(target_os = "linux") {
        println!(
            "[platform] Native container attempts require Linux; this host is {}.",
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
        "[settings] socket={socket}, namespace={namespace}, snapshotter={snapshotter}, runtime={runtime}."
    );
    println!("[settings] image={image}, network={network:?}.");
    println!(
        "[settings] Override values with SOLTI_CONTAINERD_*, SOLTI_CONTAINER_IMAGE, and SOLTI_CONTAINER_NETWORK."
    );

    let engine = Arc::new(
        ContainerdEngine::connect(
            ContainerdConfig::new(socket, namespace, snapshotter, runtime).with_network(network),
        )
        .await?,
    );
    let info = engine.probe().await?;
    println!(
        "[connect] Accepted engine={} version={} and configured plugins.",
        info.name(),
        info.version(),
    );

    let process_policy = ContainerProcessPolicy::new()
        .with_capabilities([])
        .with_no_new_privileges(true)
        .with_umask(0o077)
        .with_seccomp(SeccompPolicy::DenyHostControl);
    let runner_config = ContainerRunnerConfig::new().with_process_policy(process_policy);
    println!(
        "[policy] Replaced capabilities with an empty set and enabled noNewPrivileges, umask 077, and the host-control seccomp denylist."
    );

    let output = Arc::new(RecordingOutput::default());
    let output_handle: OutputPublisherHandle = output.clone();
    let mut router = RunnerRouter::new().with_output_publisher(output_handle);
    let engine_handle: Arc<dyn ContainerEngine> = engine;
    register_container_runner_with_config(&mut router, "containerd", engine_handle, runner_config)?;

    let mut env = TaskEnv::new();
    env.push("EXAMPLE_MESSAGE", "hello from solti container");
    let shell = r#"printf 'message=%s\n' "$EXAMPLE_MESSAGE"
printf 'diagnostic=container-stderr\n' >&2"#;
    let workload = TaskWorkload::Container(ContainerSpec::new(
        image,
        Some(vec!["/bin/sh".into()]),
        vec!["-c".into(), shell.into()],
        env,
    ));
    let spec = TaskSpec::builder("containers", workload, 120_000_u64).build()?;
    let task = Task::new("native-container", spec)?;

    let task_ref = router.build(&task).await?;
    println!(
        "[build] Built {}; no image or container operation occurred during build.",
        task_ref.name(),
    );
    task_ref.spawn(TaskContext::detached()).await?;
    println!("[attempt] Container exited with code 0 and owned resources were cleaned.");

    let events = output
        .events
        .lock()
        .expect("output recorder lock must not be poisoned");
    let mut lines = Vec::new();
    println!("[output] Published container lines:");
    for event in &*events {
        let OutputEvent::Chunk(chunk) = event else {
            continue;
        };
        let line = String::from_utf8_lossy(&chunk.line).into_owned();
        println!(
            "      attempt={} stream={:?} seq={} line={line:?}",
            chunk.attempt, chunk.stream, chunk.seq,
        );
        lines.push(line);
    }
    assert!(
        lines
            .iter()
            .any(|line| line == "message=hello from solti container")
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "diagnostic=container-stderr")
    );

    println!(
        "\nResult: one Container resource completed through containerd 2.x with hardened OCI process settings, published output, and owned-resource cleanup."
    );
    Ok(())
}
