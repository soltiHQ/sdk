#![cfg(feature = "schema")]
//! Sample types covering the shapes schemars derives, pinned as snapshots and checked by Podium.
//!
//! Each snapshot in `tests/schemas/sample-*.json` is the canonical bytes of the schema a sample
//! type derives, exactly what discovery would send. Podium's own compiler wrote the verdict next to
//! it, in `sample-*.podium.json`, with the vector runner (`tests/data/podium/out/generator.json` records the Podium commit).
//! A sample Podium refuses is a bug in the type or in the SDK's check, never a snapshot to keep.
//!
//! After a change, run the ignored `update_schema_snapshots` (it rewrites the snapshots and deletes
//! their sidecars), then the runner again.

use std::{collections::HashMap, fs, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solti_model::{WorkloadSchema, workload_schema_for};

#[derive(JsonSchema, Serialize, Deserialize)]
struct Inner {
    big: u64,
    signed: i64,
    label: Option<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct OptionsAndCollections {
    maybe: Option<String>,
    list: Vec<u32>,
    map: HashMap<String, Vec<String>>,
    nested: Inner,
    nested_list: Vec<Inner>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
enum External {
    Unit,
    Newtype(String),
    Tuple(u8, bool),
    Struct { name: String, count: Option<u32> },
}

#[derive(JsonSchema, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum Internal {
    Empty,
    Named { name: String },
    Wrapped(Inner),
}

#[derive(JsonSchema, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
enum Adjacent {
    Empty,
    Text(String),
    Pair { left: u8, right: u8 },
}

#[derive(JsonSchema, Serialize, Deserialize)]
#[serde(untagged)]
enum Untagged {
    Number(u32),
    Text(String),
    Object { name: String },
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct Newtype(String);

#[derive(JsonSchema, Serialize, Deserialize)]
struct Pair(u32, String);

#[derive(JsonSchema, Serialize, Deserialize)]
struct NewtypesAndTuples {
    newtype: Newtype,
    pair: Pair,
    tuple: (u8, String),
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct Tree {
    name: String,
    next: Option<Box<Tree>>,
    children: Vec<Tree>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct Common {
    id: String,
    tags: Vec<String>,
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct Flattened {
    own: u32,
    #[serde(flatten)]
    common: Common,
}

#[derive(JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlattenedStrict {
    own: u32,
    #[serde(flatten)]
    common: Common,
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct Scalars {
    unsigned: u64,
    signed: i64,
    float: f64,
    letter: char,
    flag: bool,
}

#[derive(JsonSchema, Serialize, Deserialize)]
struct Patterned {
    #[schemars(regex(pattern = r"^[a-z][a-z0-9-]{0,62}$"))]
    name: String,
}

/// Sample name (the file stem after `sample-`) and its derived schema.
fn samples() -> Vec<(&'static str, Value)> {
    vec![
        (
            "options-and-collections",
            workload_schema_for::<OptionsAndCollections>(),
        ),
        ("enum-external", workload_schema_for::<External>()),
        ("enum-internal", workload_schema_for::<Internal>()),
        ("enum-adjacent", workload_schema_for::<Adjacent>()),
        ("enum-untagged", workload_schema_for::<Untagged>()),
        (
            "newtypes-and-tuples",
            workload_schema_for::<NewtypesAndTuples>(),
        ),
        ("recursive", workload_schema_for::<Tree>()),
        ("flatten", workload_schema_for::<Flattened>()),
        ("flatten-deny", workload_schema_for::<FlattenedStrict>()),
        ("scalars", workload_schema_for::<Scalars>()),
        ("regex", workload_schema_for::<Patterned>()),
    ]
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/schemas")
}

fn canonical(value: &Value) -> WorkloadSchema {
    WorkloadSchema::from_value(value).expect("a sample schema is inside the SDK's subset")
}

#[test]
fn snapshots_equal_the_generated_schemas_and_podium_accepts_them() {
    for (name, value) in samples() {
        let schema = canonical(&value);
        let stem = format!("sample-{name}");
        let snapshot = fs::read(dir().join(format!("{stem}.json")))
            .unwrap_or_else(|_| panic!("{stem}.json is missing: run update_schema_snapshots"));
        assert!(
            snapshot == schema.canonical_json(),
            "{stem}.json differs from the generated schema: run update_schema_snapshots"
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

/// Integer bounds reach a nested type, which schemars puts under `$defs`.
#[test]
fn a_nested_u64_is_bounded_in_defs() {
    let value = workload_schema_for::<OptionsAndCollections>();
    let big = &value["$defs"]["Inner"]["properties"]["big"];
    assert_eq!(big["maximum"], 9_007_199_254_740_991_i64, "{big}");
    assert_eq!(big["minimum"], 0, "{big}");
    let signed = &value["$defs"]["Inner"]["properties"]["signed"];
    assert_eq!(signed["maximum"], 9_007_199_254_740_991_i64, "{signed}");
    assert_eq!(signed["minimum"], -9_007_199_254_740_991_i64, "{signed}");
}

/// Rewrites the snapshots and deletes their sidecars.
#[test]
#[ignore = "rewrites tests/schemas; then run the vector runner"]
fn update_schema_snapshots() {
    fs::create_dir_all(dir()).unwrap();
    for (name, value) in samples() {
        let stem = format!("sample-{name}");
        fs::write(
            dir().join(format!("{stem}.json")),
            canonical(&value).canonical_json(),
        )
        .unwrap();
        let _ = fs::remove_file(dir().join(format!("{stem}.podium.json")));
    }
}
