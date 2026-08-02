//! # Custom workload task: extend the SDK
//!
//! A binary can add its own workload GVK and runner.
//! Core then supervises it through the same desired-state lifecycle as built-in workloads.
//!
//! This example shows:
//!
//! - an application-owned `network.example.io/v1` `TcpProbe` workload;
//! - strict payload decoding inside the runner;
//! - runner capability introspection;
//! - routing and reconciliation through `solti-core`;
//! - one real TCP connection made by the supervised task;
//! - terminal state and retained run history.
//!
//! ```text
//! ExtensionWorkload { TcpProbe }
//!              │ exact GVK
//!              ▼
//!        RunnerRouter ──► TcpProbeRunner
//!                              │ validate JSON payload
//!                              ▼
//!                         Taskvisor task
//!                              │ connect
//!                              ▼
//!                       local TCP service
//!                              └──► terminal Task + TaskRun in core
//! ```
//!
//! Run with `cargo run -p solti --example task_custom_workload --features core`.

use std::{io, sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::json;
use solti::{
    core::{SupervisorApi, TaskWatchSubscription},
    model::{
        ExtensionWorkload, RestartPolicy, Task, TaskId, TaskManifest, TaskSpec, TaskWorkload,
        WorkloadTypeMeta,
    },
    runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter},
    taskvisor::{TaskContext, TaskError, TaskFn, TaskRef},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};
use tokio_stream::StreamExt;

const API_VERSION: &str = "network.example.io/v1";
const KIND: &str = "TcpProbe";
const WAIT_BOUND: Duration = Duration::from_secs(10);

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TcpProbeSpec {
    address: String,
    payload: String,
}

struct TcpProbeRunner;

impl Runner for TcpProbeRunner {
    fn name(&self) -> &str {
        "tcp-probe"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new(API_VERSION, KIND).expect("valid custom GVK")]
    }

    fn build_task(
        &self,
        task: &Task,
        run_id: &RunId,
        _ctx: &BuildContext,
    ) -> Result<TaskRef, RunnerError> {
        let workload = task.spec().workload();
        let TaskWorkload::Extension(extension) = workload else {
            return Err(unsupported_workload(workload));
        };
        if extension.api_version() != API_VERSION || extension.kind() != KIND {
            return Err(unsupported_workload(workload));
        }

        let spec: TcpProbeSpec = serde_json::from_value(extension.spec().clone())
            .map_err(|error| RunnerError::InvalidSpec(error.to_string()))?;
        if spec.payload.is_empty() {
            return Err(RunnerError::InvalidSpec("payload must not be empty".into()));
        }

        let address: Arc<str> = spec.address.into();
        let payload: Arc<[u8]> = spec.payload.into_bytes().into();
        Ok(TaskFn::arc(
            run_id.name().to_owned(),
            move |_ctx: TaskContext| {
                let address = Arc::clone(&address);
                let payload = Arc::clone(&payload);
                async move {
                    let mut stream = TcpStream::connect(address.as_ref())
                        .await
                        .map_err(|error| TaskError::fail(format!("connect {address}: {error}")))?;
                    stream.write_all(&payload).await.map_err(|error| {
                        TaskError::fail(format!("write probe payload: {error}"))
                    })?;
                    Ok(())
                }
            },
        ))
    }
}

fn unsupported_workload(workload: &TaskWorkload) -> RunnerError {
    RunnerError::UnsupportedWorkload {
        runner: "tcp-probe".into(),
        api_version: workload.api_version().into(),
        kind: workload.kind().into(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!(
        r#"
solti: application-owned workload

  network.example.io/v1, TcpProbe ──► TcpProbeRunner ──► core supervision
                                                               ▼
                                                        TCP connection
"#
    );
    println!(
        "[purpose] Add a real execution backend without changing solti-model, solti-runner, or solti-core."
    );

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    println!("[service] Listening for one probe at {address}.");

    let service = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await?;
        let mut payload = [0_u8; 4];
        stream.read_exact(&mut payload).await?;
        Ok::<_, io::Error>((peer, payload))
    });

    let mut router = RunnerRouter::new();
    router.register(Arc::new(TcpProbeRunner))?;
    let capabilities = router.capabilities();
    let capability = &capabilities.runners()[0];
    println!(
        "[capability] runner={} workload={}/{}.",
        capability.name(),
        capability.workload_types()[0].api_version(),
        capability.workload_types()[0].kind(),
    );

    let supervisor = SupervisorApi::builder(router).start().await?;
    let mut watch = supervisor.watch_tasks(&Default::default(), None)?;
    let workload = TaskWorkload::Extension(ExtensionWorkload::new(
        API_VERSION,
        KIND,
        json!({
            "address": address.to_string(),
            "payload": "PING"
        }),
    )?);
    let spec = TaskSpec::builder("network-probes", workload, 5_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let manifest = TaskManifest::new("local-tcp-probe", spec)?;
    let task_name = manifest.name().clone();

    let committed = supervisor.create_task(manifest).await?;
    println!(
        "[core] Committed task={} generation={} before background reconciliation.",
        committed.name(),
        committed.metadata().generation(),
    );

    let (peer, payload) = timeout(WAIT_BOUND, service).await???;
    println!(
        "[service] Received {:?} from {peer}.",
        String::from_utf8_lossy(&payload),
    );
    assert_eq!(&payload, b"PING");

    let terminal = wait_for_terminal(&mut watch, &task_name).await?;
    let runs = supervisor.list_task_runs(&task_name);
    println!(
        "[result] phase={}, retainedRuns={}.",
        terminal.status().phase(),
        runs.len(),
    );
    assert_eq!(runs.len(), 1);

    supervisor.shutdown().await?;
    println!(
        "\nResult: the custom GVK was routed, executed, observed, and retained through the standard SDK lifecycle."
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
