//! # Custom workload task: extend the SDK
//!
//! A binary can add its own workload type and runner.
//! Core then admits and supervises it through the same desired-state lifecycle as built-in workloads.
//!
//! This example shows:
//!
//! - an application-owned `network.example.io/v1` `TcpProbe` workload type;
//! - one body type that gives the schema, the admission check and the runner's decoder;
//! - runner capability introspection, with the schema fingerprint;
//! - routing and reconciliation through `solti-core`;
//! - one real TCP connection made by the supervised task;
//! - terminal state and retained run history.
//!
//! ```text
//! Workload { network.example.io/v1, TcpProbe, body }
//!              │ exact GVK
//!              ▼
//!        RunnerRouter ──► TcpProbeRunner
//!              │ admission: WorkloadDecl::of::<TcpProbeSpec>() decodes the body
//!              ▼
//!         Taskvisor task
//!              │ connect
//!              ▼
//!       local TCP service
//!              └──► terminal Task + TaskRun in core
//! ```
//!
//! Run with `cargo run -p solti --example task_custom_workload --features core,model-schema`.

use std::{io, sync::Arc, time::Duration};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use solti::{
    core::{SupervisorApi, TaskWatchSubscription},
    model::{
        RestartPolicy, Task, TaskId, TaskManifest, TaskRunQuery, TaskSpec, TaskWorkload, Workload,
        WorkloadBody, WorkloadBodyErrorKind, WorkloadDecl,
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

const WAIT_BOUND: Duration = Duration::from_secs(10);

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TcpProbeSpec {
    address: String,
    payload: String,
}

impl WorkloadBody for TcpProbeSpec {
    const API_VERSION: &'static str = "network.example.io/v1";
    const KIND: &'static str = "TcpProbe";

    fn validate(&self) -> Result<(), String> {
        if self.payload.is_empty() {
            return Err("payload must not be empty".into());
        }
        Ok(())
    }
}

struct TcpProbeRunner;

#[solti_runner::async_trait]
impl Runner for TcpProbeRunner {
    fn name(&self) -> &str {
        "tcp-probe"
    }

    fn workload_types(&self) -> Vec<WorkloadDecl> {
        vec![WorkloadDecl::of::<TcpProbeSpec>()]
    }

    async fn build_task(
        &self,
        task: &Task,
        _run_id: &RunId,
        _ctx: &BuildContext,
        _cancellation: &solti_runner::BuildCancellation,
        _scope: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let workload = task.spec().workload();
        let TaskWorkload::Routed(routed) = workload else {
            return Err(unsupported_workload(workload));
        };
        let spec = routed
            .decode::<TcpProbeSpec>()
            .map_err(|error| match error.kind() {
                WorkloadBodyErrorKind::TypeMismatch { .. } => unsupported_workload(workload),
                _ => RunnerError::InvalidSpec(error.detail()),
            })?;

        let address: Arc<str> = spec.address.into();
        let payload: Arc<[u8]> = spec.payload.into_bytes().into();
        Ok(TaskFn::arc(move |_ctx: TaskContext| {
            let address = Arc::clone(&address);
            let payload = Arc::clone(&payload);
            async move {
                let mut stream = TcpStream::connect(address.as_ref())
                    .await
                    .map_err(|error| TaskError::fail(format!("connect {address}: {error}")))?;
                stream
                    .write_all(&payload)
                    .await
                    .map_err(|error| TaskError::fail(format!("write probe payload: {error}")))?;
                Ok(())
            }
        }))
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
    let workload_type = &capability.workload_types()[0];
    println!(
        "[capability] runner={} workload={}/{} schemaFingerprint={}.",
        capability.name(),
        workload_type.api_version(),
        workload_type.kind(),
        workload_type.fingerprint(),
    );

    let supervisor = SupervisorApi::builder(router).start().await?;
    let mut watch = supervisor.watch_tasks(&Default::default(), None)?;
    let workload = TaskWorkload::Routed(Workload::new(
        TcpProbeSpec::API_VERSION,
        TcpProbeSpec::KIND,
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
    let runs = supervisor
        .query_task_runs(&task_name, &TaskRunQuery::new())?
        .ok_or_else(|| io::Error::other("task disappeared before run history was read"))?;
    println!(
        "[result] phase={}, retainedRuns={}.",
        terminal.status().phase(),
        runs.items.len(),
    );
    assert_eq!(runs.items.len(), 1);

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
