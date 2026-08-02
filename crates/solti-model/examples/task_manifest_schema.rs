//! # Task manifest schema
//!
//! This example builds a real `TaskManifest`.
//! It generates and prints the matching JSON Schema.
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
    let manifest = task_manifest()?;

    // Runtime validation checks the complete model contract.
    manifest.validate()?;

    let document = serde_json::to_value(&manifest)?;
    let schema = serde_json::to_value(schemars::schema_for!(TaskManifest))?;

    jsonschema::meta::validate(&schema)
        .map_err(|error| format!("generated JSON Schema is invalid: {error}"))?;
    let validator = jsonschema::validator_for(&schema)?;
    validator
        .validate(&document)
        .map_err(|error| format!("manifest does not match its JSON Schema: {error}"))?;

    println!("TaskManifest:");
    println!("{}", serde_json::to_string_pretty(&document)?);
    println!("\nTaskManifest JSON Schema:");
    println!("{}", serde_json::to_string_pretty(&schema)?);
    println!("\nRuntime validation: passed");
    println!("JSON Schema validation: passed");

    Ok(())
}
