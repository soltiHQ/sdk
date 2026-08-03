//! # Shared Prometheus registry
//!
//! An application owns one Prometheus registry.
//! Solti collectors register into that registry.
//! The application decides how to expose gathered metrics.
//!
//! This example shows:
//!
//! - the default feature set;
//! - build information with constant labels;
//! - registry gathering and text encoding;
//! - duplicate registration rejection;
//! - the boundary before an HTTP endpoint.
//!
//! Run with `cargo run -p solti-prometheus --example shared_registry`.

use prometheus::{Encoder, TextEncoder};
use solti_prometheus::{Registry, register_build_info};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-prometheus: shared registry

  application
      ├──► Registry::new()
      │
      └── constant build labels ──► register_build_info()
                                              │
                                              ▼
                                      shared Registry
                                              │ gather()
                                              ▼
                                        metric families
                                              │ TextEncoder
                                              ▼
                                   Prometheus exposition text

  The application owns the registry and its HTTP transport.
  Registration adds collectors; it does not start a server.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Create one application-owned registry and turn its metrics into Prometheus text."
    );

    let registry = Registry::new();
    println!("[registry] Created an empty registry owned by this application.");

    register_build_info(
        &registry,
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("component", "example-agent"),
        ],
    )?;
    println!("[registration] Added solti_build_info with constant version and component labels.");

    let families = registry.gather();
    println!(
        "[gather] The registry returned {} metric family.",
        families.len(),
    );
    assert_eq!(families.len(), 1);

    let mut buffer = Vec::new();
    TextEncoder::new().encode(&families, &mut buffer)?;
    let exposition = String::from_utf8(buffer)?;
    let sample = exposition
        .lines()
        .find(|line| line.starts_with("solti_build_info"))
        .ok_or("solti_build_info sample is missing")?;
    println!("[exposition] {sample}");

    let duplicate = register_build_info(
        &registry,
        &[
            ("version", env!("CARGO_PKG_VERSION")),
            ("component", "example-agent"),
        ],
    );
    assert!(duplicate.is_err());
    println!("[registration] A second collector with the same descriptor is rejected.");

    println!(
        "\nResult: the registry is ready for an application-owned exporter or the optional server task."
    );
    Ok(())
}
