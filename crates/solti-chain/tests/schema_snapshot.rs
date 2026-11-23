//! The built-in `chain.solti.io/v1alpha1` schema, pinned as a snapshot.
//!
//! `tests/schemas/chain.solti.io-v1alpha1-Chain.json` holds the canonical bytes the chain runner's
//! declaration sends in discovery. After a type change, run the ignored `update_schema_snapshots`.

use std::{fs, path::PathBuf};

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
fn snapshot_equals_what_discovery_sends() {
    let schema = sent();
    let snapshot = fs::read(dir().join(format!("{STEM}.json")))
        .expect("the snapshot is missing: run update_schema_snapshots");
    assert!(
        snapshot == schema.canonical_json(),
        "{STEM}.json differs from the declared schema: run update_schema_snapshots"
    );
}

/// Rewrites the snapshot.
#[test]
#[ignore = "rewrites tests/schemas"]
fn update_schema_snapshots() {
    fs::create_dir_all(dir()).unwrap();
    fs::write(dir().join(format!("{STEM}.json")), sent().canonical_json()).unwrap();
}
