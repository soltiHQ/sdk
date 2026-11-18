//! The built-in `chain.solti.io/v1alpha1` schema, pinned as a snapshot and checked by Podium.
//!
//! `tests/schemas/chain.solti.io-v1alpha1-Chain.json` holds the canonical bytes the chain runner's
//! declaration sends in discovery. Podium's own compiler wrote its verdict next to it, in
//! `*.podium.json`, through the vector runner. A refused built-in is a bug, never a snapshot to keep.
//!
//! After a type change, run the ignored `update_schema_snapshots` (it rewrites the snapshot and
//! deletes its sidecar), then the runner again.

use std::{fs, path::PathBuf};

use serde_json::Value;
use solti_chain::{CHAIN_API_VERSION, CHAIN_KIND, ChainRunner};
use solti_model::WorkloadSchema;
use solti_runner::{Runner, RunnerRouter};

const STEM: &str = "chain.solti.io-v1alpha1-Chain";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/schemas")
}

fn sent() -> WorkloadSchema {
    let runner = ChainRunner::new("snapshot", RunnerRouter::new().catalog());
    let decl = runner.workload_types().remove(0);
    assert_eq!(decl.api_version(), CHAIN_API_VERSION);
    assert_eq!(decl.kind(), CHAIN_KIND);
    decl.workload_type()
        .expect("the built-in schema is inside the SDK's subset")
        .schema()
        .clone()
}

#[test]
fn snapshot_equals_what_discovery_sends_and_podium_accepts_it() {
    let schema = sent();
    let snapshot = fs::read(dir().join(format!("{STEM}.json")))
        .expect("the snapshot is missing: run update_schema_snapshots");
    assert!(
        snapshot == schema.canonical_json(),
        "{STEM}.json differs from the declared schema: run update_schema_snapshots"
    );
    let sidecar: Value = serde_json::from_slice(
        &fs::read(dir().join(format!("{STEM}.podium.json")))
            .expect("the sidecar is missing: run the vector runner"),
    )
    .expect("the sidecar is JSON");
    assert_eq!(
        sidecar["fingerprint"].as_str(),
        Some(schema.fingerprint().as_str()),
        "the sidecar's fingerprint is stale"
    );
    assert_eq!(
        sidecar["podium"]["compile"].as_str(),
        Some("ok"),
        "Podium refuses it: {}",
        sidecar["podium"]["error"]
    );
}

/// Rewrites the snapshot and deletes its sidecar.
#[test]
#[ignore = "rewrites tests/schemas; then run the vector runner"]
fn update_schema_snapshots() {
    fs::create_dir_all(dir()).unwrap();
    fs::write(dir().join(format!("{STEM}.json")), sent().canonical_json()).unwrap();
    let _ = fs::remove_file(dir().join(format!("{STEM}.podium.json")));
}
