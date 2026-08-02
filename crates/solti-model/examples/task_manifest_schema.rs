//! # Task manifest schema
//!
//! This example builds a real `TaskManifest`.
//! It generates the matching JSON Schema.
//! It prints the schema facts that matter to a consumer.
//! It also validates the serialized manifest against that schema.
//!
//! JSON Schema describes the serialized contract.
//! `TaskManifest::validate` remains authoritative for runtime model invariants.
//! Deserialization also applies the runtime model checks.
//!
//! Run with `cargo run -p solti-model --example task_manifest_schema`.

use solti_model::{
    Flag, Labels, ModelError, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest, TaskSpec,
    TaskWorkload,
};

const FLOW: &str = r#"
solti-model: runtime model and JSON Schema

  TaskManifest value ──► validate() ────────────────┐
          └──► serde JSON document ─────────────────┤
                                                    ├──► validation passed
  TaskManifest type ──► schemars::schema_for!() ────┤
                              └──► JSON Schema ─────┘

  Runtime validation remains authoritative for semantic invariants.
  JSON Schema describes and validates the serialized structure.
"#;

fn task_manifest() -> Result<TaskManifest, ModelError> {
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "thumbnail-worker".into(),
            args: vec!["--source".into(), "cover.png".into()],
        },
        TaskEnv::default(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("media", workload, 30_000_u64).build()?;

    let mut labels = Labels::new();
    labels.insert("app.kubernetes.io/name", "thumbnail-worker");

    TaskManifest::new("thumbnail-cover", spec)?.with_labels(labels)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("{FLOW}");
    println!(
        "[purpose] Generate the wire schema and verify that one real manifest satisfies both contracts."
    );

    let manifest = task_manifest()?;

    manifest.validate()?;
    println!("[runtime] TaskManifest::validate accepted the complete desired state.");

    let document = serde_json::to_value(&manifest)?;
    let schema = serde_json::to_value(schemars::schema_for!(TaskManifest))?;

    jsonschema::meta::validate(&schema)
        .map_err(|error| format!("generated JSON Schema is invalid: {error}"))?;
    let validator = jsonschema::validator_for(&schema)?;
    validator
        .validate(&document)
        .map_err(|error| format!("manifest does not match its JSON Schema: {error}"))?;

    let definitions = schema
        .get("$defs")
        .and_then(serde_json::Value::as_object)
        .ok_or("TaskManifest schema has no $defs object")?;
    let workload_variants = schema
        .pointer("/$defs/TaskWorkload/oneOf")
        .and_then(serde_json::Value::as_array)
        .ok_or("TaskWorkload schema has no oneOf variants")?;
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .ok_or("TaskManifest schema has no required fields")?;
    let required_names: Vec<&str> = required
        .iter()
        .map(|field| {
            field
                .as_str()
                .ok_or("TaskManifest required field must be a string")
        })
        .collect::<Result<_, _>>()?;
    let draft = schema
        .get("$schema")
        .and_then(serde_json::Value::as_str)
        .ok_or("TaskManifest schema has no draft identifier")?;
    let title = schema
        .get("title")
        .and_then(serde_json::Value::as_str)
        .ok_or("TaskManifest schema has no title")?;

    println!("[document] Serialized TaskManifest:");
    println!("{}", serde_json::to_string_pretty(&document)?);
    println!("[schema] draft={draft}");
    println!("[schema] title={title}");
    println!(
        "[schema] root required fields={}",
        required_names.join(", ")
    );
    println!("[schema] reusable definitions={}", definitions.len());
    println!("[schema] TaskWorkload variants={}", workload_variants.len());
    assert_eq!(workload_variants.len(), 5);

    let mut unknown = document.clone();
    unknown
        .as_object_mut()
        .ok_or("TaskManifest document must be an object")?
        .insert("unexpected".into(), true.into());
    assert!(validator.validate(&unknown).is_err());
    assert!(serde_json::from_value::<TaskManifest>(unknown).is_err());
    println!("[strictness] JSON Schema and runtime deserialization reject an unknown field.");

    println!(
        "\nResult: runtime validation and the generated JSON Schema agree on the serialized manifest."
    );

    Ok(())
}
