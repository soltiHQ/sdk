//! # HTTP agent with discovery
//!
//! This example assembles an inbound Task API and an outbound discovery heartbeat.
//! The runner registry is the source of the capabilities sent to the control plane.
//!
//! This example shows:
//!
//! - subprocess runner registration;
//! - automatic capability snapshot creation;
//! - an HTTP Task API backed by core;
//! - a supervised embedded discovery task;
//! - independent inbound and outbound endpoints;
//! - graceful shutdown of transport and SDK-owned workers.
//!
//! ```text
//! RunnerRouter ──► capabilities ───────────────┐
//!      ▼                                       ▼
//! SupervisorApi ◄── embedded discovery TaskRef + manifest
//!      │                                       │ heartbeat
//!      ▼                                       ▼
//! HTTP Task API                         external control plane
//! inbound :8085                         outbound sync endpoint
//! ```
//!
//! The control plane must implement discovery HTTP v1.
//! `SOLTI_CONTROL_PLANE` defaults to `http://127.0.0.1:8090`.
//! `SOLTI_AGENT_ADDR` defaults to `127.0.0.1:8085`; port `0` selects a free port.
//! `SOLTI_AGENT_ID` and `SOLTI_AGENT_NAME` override the example identity.
//! `SOLTI_AGENT_ADVERTISE_URL` overrides the reachable HTTP base URL; wildcard
//! binds require this value. See `agent_podium` for a complete Podium walkthrough.
//!
//! Run with `cargo run -p solti --example agent_http_discovery --features api-core-adapter,api-http,discover-http,exec-subprocess`.

#[path = "support/http_agent.rs"]
mod http_agent;

use http_agent::{Defaults, ExampleResult, Settings};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!(
        r#"
solti: discovered HTTP agent

  control plane <- discovery heartbeat <- embedded task
  API clients -> HTTP Task API -> adapter -> core -> subprocess runner
"#
    );
    println!(
        "[purpose] Advertise the exact runner capabilities served by one live HTTP task agent."
    );
    http_agent::run(Settings::from_env(Defaults {
        agent_id: "umbrella-example-agent",
        agent_name: "Umbrella example agent",
        control_plane: "http://127.0.0.1:8090",
        api_address: "127.0.0.1:8085",
        interval_ms: 10_000,
    })?)
    .await
}
