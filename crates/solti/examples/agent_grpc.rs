//! # gRPC agent: call a real supervised backend
//!
//! One process starts a subprocess runner, core supervisor, gRPC Task API, and generated client.
//! The resource is created through core and read back through the public protobuf contract.
//!
//! This example shows:
//!
//! - one `SupervisorApi` shared with `SupervisorApiAdapter`;
//! - a generated tonic service and client;
//! - bearer metadata authentication;
//! - a real retained `Subprocess` resource;
//! - protobuf conversion at the public boundary;
//! - ready-to-run `grpcurl` calls for every RPC;
//! - graceful transport and supervisor shutdown.
//!
//! ```text
//! Subprocess manifest ──► SupervisorApi ──► subprocess runner
//!                              ▼
//!                   SupervisorApiAdapter
//!                              │ domain values
//!                              ▼
//! generated gRPC service ◄── generated client
//!                    protobuf + bearer metadata
//! ```
//!
//! Run with `cargo run -p solti --example agent_grpc --features api-core-adapter,api-grpc,exec-subprocess`.

use std::{env, io, sync::Arc, time::Duration};

use solti::{
    api::{
        GRPC_API_SERVICE, GrpcApi, SupervisorApiAdapter,
        grpc::wire::{ListTasksRequest, TaskServiceClient},
        tonic::{Request, transport::Server},
    },
    core::SupervisorApi,
    exec::subprocess::register_subprocess_runner,
    model::{
        Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest, TaskSpec,
        TaskWorkload, Token,
    },
    runner::RunnerRouter,
};
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;

const CHILD_MODE: &str = "--grpc-agent-child";
const TOKEN: &str = "umbrella-example-token";
const AUTHORIZATION: &str = "Bearer umbrella-example-token";

const GRPC_COMMANDS: &str = r#"
[commands] Run these from the SDK repository root in a second terminal.
[commands] They require grpcurl and use the bearer token accepted by this example.

ADDRESS='__ADDRESS__'
REQUEST='/tmp/solti-grpc-task.json'
GRPCURL=(
  grpcurl
  -plaintext
  -import-path crates/solti-api/proto
  -proto solti/task/v1/api.proto
  -H 'authorization: Bearer umbrella-example-token'
)

# Prepare one protobuf-JSON CreateTask or ApplyTask request.
cat >"$REQUEST" <<'JSON'
{
  "manifest": {
    "apiVersion": "solti.io/v1",
    "kind": "Task",
    "metadata": {
      "name": "grpc-demo",
      "labels": {
        "example": "grpc-agent"
      }
    },
    "spec": {
      "slot": "grpc-shell",
      "workload": {
        "apiVersion": "solti.io/v1",
        "kind": "Subprocess",
        "subprocess": {
          "mode": {
            "command": {
              "command": "/bin/sh",
              "args": [
                "-c",
                "for i in $(seq 1 30); do sleep 2; echo tick=$i; done"
              ]
            }
          },
          "failOnNonZero": true
        }
      },
      "timeoutMs": "90000",
      "restart": "RESTART_POLICY_NEVER",
      "backoff": {
        "jitter": "JITTER_POLICY_FULL",
        "firstMs": "1000",
        "maxMs": "30000",
        "factor": 2.0
      },
      "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
    }
  }
}
JSON

# Create the task.
"${GRPCURL[@]}" -d @ \
  "$ADDRESS" solti.task.v1.TaskService/CreateTask \
  <"$REQUEST"

# Stream live output while the one-minute task is running.
# OutputChunk.line is protobuf bytes and appears as base64 in JSON.
"${GRPCURL[@]}" -d '{"name":"grpc-demo"}' \
  "$ADDRESS" solti.task.v1.TaskService/StreamTaskLogs

# Read the resource and a filtered collection page.
"${GRPCURL[@]}" -d '{"name":"grpc-demo"}' \
  "$ADDRESS" solti.task.v1.TaskService/GetTask
"${GRPCURL[@]}" -d '{"slot":"grpc-shell","limit":10}' \
  "$ADDRESS" solti.task.v1.TaskService/ListTasks

# Apply the same desired state. This apply is a no-op.
"${GRPCURL[@]}" -d @ \
  "$ADDRESS" solti.task.v1.TaskService/ApplyTask \
  <"$REQUEST"

# Read retained attempt history after the command finishes.
"${GRPCURL[@]}" -d '{"name":"grpc-demo"}' \
  "$ADDRESS" solti.task.v1.TaskService/ListTaskRuns

# Watch current objects and later changes. This runs until Ctrl-C.
"${GRPCURL[@]}" -d '{"resourceVersion":"0"}' \
  "$ADDRESS" solti.task.v1.TaskService/WatchTasks

# Delete the resource and its retained history.
"${GRPCURL[@]}" -d '{"name":"grpc-demo"}' \
  "$ADDRESS" solti.task.v1.TaskService/DeleteTask
"#;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    if env::args().nth(1).as_deref() == Some(CHILD_MODE) {
        println!("hello from the gRPC agent subprocess");
        return Ok(());
    }

    println!(
        r#"
solti: gRPC task agent

  generated client ──► gRPC Task API ──► adapter ──► core ──► subprocess
        bearer             protobuf        domain      │
                                                      └──► retained Task
"#
    );
    println!(
        "[purpose] Prove that a generated client reads the same resource supervised by the in-process SDK."
    );

    let mut router = RunnerRouter::new();
    let subprocess_runner = register_subprocess_runner(&mut router, "default")?;
    let supervisor = Arc::new(SupervisorApi::builder(router).start().await?);

    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: current_executable()?,
            args: vec![CHILD_MODE.into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("grpc-example", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    let committed = supervisor
        .create_task(TaskManifest::new("grpc-subprocess", spec)?)
        .await?;
    println!(
        "[core] Committed task={} generation={}.",
        committed.name(),
        committed.metadata().generation(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
    let token = Token::new(TOKEN)?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(GrpcApi::new(handler).with_auth(token).server())
            .serve_with_incoming_shutdown(incoming, async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    println!("[server] service={GRPC_API_SERVICE}, address={address}.");

    let mut client = TaskServiceClient::connect(format!("http://{address}")).await?;
    let mut request = Request::new(ListTasksRequest {
        slot: Some("grpc-example".into()),
        phases: Vec::new(),
        limit: 10,
        label_selector: String::new(),
        r#continue: String::new(),
    });
    request
        .metadata_mut()
        .insert("authorization", AUTHORIZATION.parse()?);
    let page = client.list_tasks(request).await?.into_inner();
    let task = page
        .tasks
        .first()
        .ok_or_else(|| io::Error::other("gRPC ListTasks returned no task"))?;
    let metadata = task
        .metadata
        .as_ref()
        .ok_or_else(|| io::Error::other("gRPC task has no metadata"))?;
    let workload = task
        .spec
        .as_ref()
        .and_then(|spec| spec.workload.as_ref())
        .ok_or_else(|| io::Error::other("gRPC task has no workload"))?;
    println!(
        "[client] tasks={}, name={}, workload={}/{}.",
        page.tasks.len(),
        metadata.name,
        workload.api_version,
        workload.kind,
    );
    assert_eq!(metadata.name, "grpc-subprocess");

    print_grpc_commands(address);
    println!("[shutdown] Press Ctrl-C in the agent terminal to stop.");
    tokio::signal::ctrl_c().await?;

    let _ = shutdown_tx.send(());
    server.await??;
    supervisor.shutdown().await?;
    subprocess_runner.shutdown(Duration::from_secs(5)).await?;
    println!(
        "\nResult: the public gRPC contract returned a resource owned and executed by the core supervisor."
    );
    Ok(())
}

fn print_grpc_commands(address: std::net::SocketAddr) {
    println!(
        "{}",
        GRPC_COMMANDS.replace("__ADDRESS__", &address.to_string())
    );
}

fn current_executable() -> ExampleResult<String> {
    env::current_exe()?
        .into_os_string()
        .into_string()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "example path is not UTF-8").into())
}
