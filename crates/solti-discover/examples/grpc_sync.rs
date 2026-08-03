//! # gRPC discovery sync
//!
//! The discovery task creates its gRPC channel lazily.
//! Building the task performs no network connection.
//!
//! This example shows:
//!
//! - independent advertised and discovery transports;
//! - a complete gRPC discovery task;
//! - the fixed discovery v1 service identity;
//! - lazy connection behavior;
//! - an optional real sync against an external control plane.
//!
//! The default run builds and inspects the task without contacting a server.
//! Pass `--send` when the configured endpoint implements discovery gRPC v1.
//!
//! Run with `cargo run -p solti-discover --example grpc_sync --features grpc`.
//! Send with `cargo run -p solti-discover --example grpc_sync --features grpc -- --send`.

use std::sync::Arc;

use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DISCOVERY_GRPC_SERVICE,
    DISCOVERY_PROTOCOL_VERSION, DiscoverConfig, DiscoveryTransport, MonotonicUptime, sync,
};
use solti_model::{
    AgentCapabilities, AgentId, Labels, RunnerCapability, TaskWorkload, WORKLOAD_API_VERSION,
    WorkloadTypeMeta,
};
use taskvisor::TaskContext;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-discover: lazy gRPC discovery client

  advertised AgentEndpoint
  HTTP Task API v1 ───────────────────────────┐
                                              │ inside SyncRequest
  identity + capabilities + uptime ───────────┤
                                              ▼
                                        DiscoverConfig
                                              │ sync()
                                              ▼
                              TaskManifest + embedded TaskRef
                                              │ build only: no connection
                                              │
                                              ▼
  external control plane ◄── gRPC DiscoverService/Sync
          └── SyncResponse ──────────────┘

  The agent may expose HTTP while discovery uses gRPC.
  A successful channel is reused by later attempts of the same task.
"#;

fn capabilities() -> Result<AgentCapabilities, solti_model::ModelError> {
    let mut labels = Labels::new();
    labels.insert("solti.io/runner-name", "containerd");
    AgentCapabilities::new(vec![RunnerCapability::new(
        "containerd",
        labels,
        vec![WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Container")?],
    )?])
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Build a gRPC discovery task, prove connection is lazy, and optionally send one real heartbeat."
    );

    let endpoint = std::env::var("SOLTI_DISCOVERY_GRPC_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:50051".into());
    println!(
        "[contract] discoveryVersion=v{DISCOVERY_PROTOCOL_VERSION}, service={DISCOVERY_GRPC_SERVICE}."
    );
    println!("[settings] controlPlane={endpoint}.");
    println!("[settings] Override it with SOLTI_DISCOVERY_GRPC_ENDPOINT.");

    let config = DiscoverConfig::builder(
        AgentId::new("agent-grpc-1")?,
        "Agent gRPC 1",
        AgentEndpoint::new("http://127.0.0.1:8085", AgentEndpointType::Http, 1)?,
        ControlPlaneEndpoint::new(endpoint, DiscoveryTransport::Grpc)?,
        1,
        "grpc-discovery@1",
    )
    .capabilities(capabilities()?)
    .connect_timeout_ms(5_000)
    .request_timeout_ms(10_000)
    .build()?;
    println!("[config] Advertised HTTP Task API v1; selected gRPC for outbound discovery.");

    let (manifest, task_ref) = sync(config, Arc::new(MonotonicUptime::new()))?;
    let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
        return Err("discovery manifest is not Embedded".into());
    };
    println!(
        "[build] task={}, slot={}, revision={}, taskvisorName={}.",
        manifest.name(),
        manifest.spec().slot(),
        embedded.revision(),
        task_ref.name(),
    );
    println!("[build] The gRPC channel has not been created.");

    let send = std::env::args()
        .skip(1)
        .any(|argument| argument == "--send");
    if send {
        println!("[send] Starting one attempt and connecting to the configured control plane.");
        task_ref.spawn(TaskContext::detached()).await?;
        println!("\nResult: the external control plane accepted one gRPC discovery v1 heartbeat.");
    } else {
        println!(
            "[send] Skipped. Pass --send only when the endpoint implements discovery gRPC v1."
        );
        println!(
            "\nResult: the embedded gRPC task and manifest were built without opening a network connection."
        );
    }
    Ok(())
}
