#![cfg(all(feature = "subprocess", feature = "container"))]
//! The built-in `exec.solti.io/v1` schemas, pinned as snapshots and checked by Podium.
//!
//! `tests/schemas/<group>-<version>-<kind>.json` holds the canonical bytes a runner's declaration
//! sends in discovery. Podium's own compiler wrote its verdict next to each, in `*.podium.json`,
//! through the vector runner. A refused built-in is a bug, never a snapshot to keep.
//!
//! After a type change, run the ignored `update_schema_snapshots` (it rewrites the snapshots and
//! deletes their sidecars), then the runner again.

use std::{fs, path::PathBuf};

use serde_json::{Value, json};
use solti_exec::{
    EXEC_API_VERSION,
    container::{CONTAINER_KIND, ContainerSpec},
    subprocess::{SUBPROCESS_KIND, SubprocessMode, SubprocessRunner, SubprocessSpec},
};
use solti_model::{Flag, TaskEnv, Workload, WorkloadDecl, WorkloadSchema};
use solti_runner::Runner;

/// File stem and the declaration discovery sends.
fn declarations() -> Vec<(String, WorkloadDecl)> {
    let subprocess = SubprocessRunner::new("snapshot")
        .unwrap()
        .workload_types()
        .remove(0);
    vec![
        (format!("exec.solti.io-v1-{SUBPROCESS_KIND}"), subprocess),
        (
            format!("exec.solti.io-v1-{CONTAINER_KIND}"),
            WorkloadDecl::of::<ContainerSpec>(),
        ),
    ]
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/schemas")
}

fn sent(decl: &WorkloadDecl) -> WorkloadSchema {
    decl.workload_type()
        .expect("a built-in schema is inside the SDK's subset")
        .schema()
        .clone()
}

#[test]
fn the_declarations_are_the_exec_types() {
    let names: Vec<_> = declarations()
        .iter()
        .map(|(_, decl)| format!("{}/{}", decl.api_version(), decl.kind()))
        .collect();
    assert_eq!(
        names,
        [
            format!("{EXEC_API_VERSION}/Subprocess"),
            format!("{EXEC_API_VERSION}/Container")
        ]
    );
}

#[test]
fn snapshots_equal_what_discovery_sends_and_podium_accepts_them() {
    for (stem, decl) in declarations() {
        let schema = sent(&decl);
        let snapshot = fs::read(dir().join(format!("{stem}.json")))
            .unwrap_or_else(|_| panic!("{stem}.json is missing: run update_schema_snapshots"));
        assert!(
            snapshot == schema.canonical_json(),
            "{stem}.json differs from the declared schema: run update_schema_snapshots"
        );
        let sidecar: Value = serde_json::from_slice(
            &fs::read(dir().join(format!("{stem}.podium.json")))
                .unwrap_or_else(|_| panic!("{stem}.podium.json is missing: run the vector runner")),
        )
        .expect("the sidecar is JSON");
        assert_eq!(
            sidecar["fingerprint"].as_str(),
            Some(schema.fingerprint().as_str()),
            "{stem}: the sidecar's fingerprint is stale"
        );
        assert_eq!(
            sidecar["podium"]["compile"].as_str(),
            Some("ok"),
            "{stem}: Podium refuses it: {}",
            sidecar["podium"]["error"]
        );
    }
}

/// Checks `body` against the schema `decl` emits, then with the declaration's own check.
///
/// A body serde accepts but its schema refuses would be refused by Podium at spec create.
fn assert_valid(decl: &WorkloadDecl, body: Value) {
    let schema: Value =
        serde_json::from_slice(sent(decl).canonical_json()).expect("the canonical JSON parses");
    jsonschema::meta::validate(&schema).expect("a draft 2020-12 schema");
    let validator = jsonschema::validator_for(&schema).expect("the schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&body)
        .map(|error| format!("{} at '{}'", error, error.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{body} fails the {} schema it emits: {errors:?}",
        decl.kind()
    );
    let workload = Workload::new(decl.api_version(), decl.kind(), body.clone()).unwrap();
    decl.check(&workload)
        .unwrap_or_else(|reason| panic!("{body} fails the {} check: {reason}", decl.kind()));
}

#[test]
fn valid_bodies_validate_against_the_schemas_their_declarations_emit() {
    let mut declarations = declarations().into_iter().map(|(_, decl)| decl);
    let subprocess = declarations.next().unwrap();
    let container = declarations.next().unwrap();

    // What an SDK client sends: the bodies the Rust types serialize to.
    let command = SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello".into(), "a=b".into()],
        },
        TaskEnv::single("A", "1"),
        Some(PathBuf::from("/work")),
        Flag::disabled(),
    );
    let script = SubprocessSpec::new(
        SubprocessMode::Script {
            interpreter: "bash".into(),
            body: "ZWNobyBoaQ==".into(),
            args: vec!["x".into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    );
    let image = ContainerSpec::new(
        "docker.io/library/redis:7".into(),
        Some(vec!["sh".into()]),
        vec!["-c".into(), "true".into()],
        TaskEnv::single("A", "1"),
    );
    for (decl, body) in [
        (
            &subprocess,
            Workload::from_body(&command).unwrap().body().clone(),
        ),
        (
            &subprocess,
            Workload::from_body(&script).unwrap().body().clone(),
        ),
        (
            &container,
            Workload::from_body(&image).unwrap().body().clone(),
        ),
    ] {
        assert_valid(decl, body);
    }

    // What a remote client may write: the smallest bodies, defaults left out.
    assert_valid(
        &subprocess,
        json!({"mode": {"command": {"command": "echo"}}}),
    );
    assert_valid(
        &subprocess,
        json!({"mode": {"script": {"interpreter": "bash", "body": "ZWNobyBoaQ=="}}}),
    );
    assert_valid(&container, json!({"image": "redis:7"}));
    assert_valid(&container, json!({"image": "redis:7", "command": null}));
}

/// Rewrites the snapshots and deletes their sidecars.
#[test]
#[ignore = "rewrites tests/schemas; then run the vector runner"]
fn update_schema_snapshots() {
    fs::create_dir_all(dir()).unwrap();
    for (stem, decl) in declarations() {
        fs::write(
            dir().join(format!("{stem}.json")),
            sent(&decl).canonical_json(),
        )
        .unwrap();
        let _ = fs::remove_file(dir().join(format!("{stem}.podium.json")));
    }
}
