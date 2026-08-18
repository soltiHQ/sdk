//! # Supervised metrics server
//!
//! The optional server integration turns a shared registry into an embedded task.
//! `solti-core` can submit the returned manifest and `TaskRef` together.
//!
//! This example shows:
//!
//! - the inputs accepted by `server`;
//! - the returned desired-state manifest;
//! - the matching Taskvisor task;
//! - the composed revision containing the listen address;
//! - the boundary before binding and serving HTTP.
//!
//! Run with `cargo run -p solti-prometheus --example metrics_server --features server`.

use std::sync::Arc;

use solti_model::TaskWorkload;
use solti_prometheus::{METRICS_SERVER_SLOT, Registry, register_build_info, server};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-prometheus: supervised metrics server

  shared Registry + listen address + caller revision
                            ▼
                         server()
                            ├──► TaskManifest (desired lifecycle) ─┐
                            └──► TaskRef (implementation) ─────────┤
                                                                   ▼
                                                   solti-core embedded-task API
                                                                   │ submit
                                                                   ▼
                                                          Taskvisor attempt
                                                                   └──► bind address ──► GET /metrics

  server() only constructs both values.
  The address is bound after the TaskRef is submitted and executed.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Package a registry endpoint as desired state plus its in-process implementation."
    );

    let registry = Arc::new(Registry::new());
    register_build_info(&registry, &[("component", "example-agent")])?;
    println!("[registry] The shared registry already contains build information.");

    let address = "127.0.0.1:9090";
    let caller_revision = "example-agent-v1";
    println!("[input] address={address}, caller revision={caller_revision}.");

    let (manifest, _task_ref) = server(Arc::clone(&registry), address, caller_revision)?;
    let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
        return Err("metrics server must produce an Embedded workload".into());
    };

    println!(
        "[manifest] name={}, slot={}, workload=Embedded.",
        manifest.name(),
        manifest.slot(),
    );
    println!("[manifest] composed revision={}.", embedded.revision());
    println!(
        "[manifest] restart={:?}, admission={:?}.",
        manifest.spec().restart(),
        manifest.spec().admission(),
    );
    println!(
        "[task] Reusable TaskRef built; Taskvisor assigns its registration name through TaskSpec."
    );

    assert_eq!(manifest.name().as_str(), METRICS_SERVER_SLOT);
    assert_eq!(manifest.slot().as_str(), METRICS_SERVER_SLOT);
    assert_eq!(embedded.revision(), "example-agent-v1|addr=127.0.0.1:9090");

    println!("[runtime] No socket was bound; this example did not submit the TaskRef.");
    println!(
        "\nResult: the manifest and TaskRef are ready for the solti-core embedded-task boundary."
    );
    Ok(())
}
