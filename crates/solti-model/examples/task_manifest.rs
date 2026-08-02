//! # Task manifest
//!
//! `TaskManifest` is caller-owned desired state.
//! It contains no UID, resource version, generation, creation time, or status.
//!
//! This example shows:
//!
//! - subprocess desired state;
//! - labels, annotations, environment, policies, and runner selection;
//! - validation at the manifest boundary;
//! - the serialized API shape;
//! - strict rejection of unknown fields.
//!
//! Run with `cargo run -p solti-model --example task_manifest`.

use std::{num::NonZeroU32, path::PathBuf};

use solti_model::{
    AdmissionPolicy, Annotations, BackoffPolicy, Flag, JitterPolicy, LabelSelector, Labels,
    RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest, TaskSpec, TaskWorkload,
};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-model: caller-owned desired state

  workload + slot + timeout ─┐
  restart + backoff ─────────┤
  admission ─────────────────┼──► TaskSpec::builder().build()
  runnerSelector ────────────┘                 │
  metadata.name ───────────────────────────────┤
                                               ▼
                                    TaskManifest::new()
                                               ├──► with_labels()
                                               ├──► with_annotations()
                                               └──► validate()
                                                        │
                                                        ▼
                                               serialized resource
                                                        └──► create / apply

  Server-owned metadata and status are deliberately absent.
  Unknown resource fields are rejected during deserialization.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Build the complete desired-state document that an API client creates or applies."
    );

    let mut env = TaskEnv::new();
    env.push("SOURCE", "cover.png");
    env.push("OUTPUT", "cover.webp");
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "thumbnail-worker".into(),
            args: vec!["--width".into(), "1280".into()],
        },
        env,
        Some(PathBuf::from("/srv/media")),
        Flag::enabled(),
    ));

    let backoff = BackoffPolicy {
        jitter: JitterPolicy::Equal,
        first_ms: 500,
        max_ms: 10_000,
        factor: 2.0,
    };
    let selector: LabelSelector = "accelerator=gpu".parse()?;
    let spec = TaskSpec::builder("media", workload, 30_000_u64)
        .restart(RestartPolicy::OnFailure)
        .backoff(backoff)
        .admission(AdmissionPolicy::Replace)
        .max_retries(NonZeroU32::new(3))
        .runner_selector(selector)
        .build()?;
    println!("[spec] Built and validated the workload, policies, timeout, and selector.");

    let mut labels = Labels::new();
    labels.insert("app.kubernetes.io/name", "thumbnail-worker");
    labels.insert("environment", "production");
    let mut annotations = Annotations::new();
    annotations.insert("example.io/owner", "media-team");

    let manifest = TaskManifest::new("thumbnail-cover", spec)?
        .with_labels(labels)?
        .with_annotations(annotations)?;
    manifest.validate()?;
    println!("[manifest] Caller-owned metadata and desired state passed runtime validation.");

    let document = serde_json::to_value(&manifest)?;
    let round_trip: TaskManifest = serde_json::from_value(document.clone())?;
    assert_eq!(round_trip, manifest);
    println!("[wire] Serialization and strict deserialization preserve the manifest:");
    println!("{}", serde_json::to_string_pretty(&document)?);

    let mut unknown = document;
    unknown
        .as_object_mut()
        .ok_or("TaskManifest must serialize as an object")?
        .insert("unexpected".into(), true.into());
    let error = serde_json::from_value::<TaskManifest>(unknown)
        .expect_err("unknown top-level fields must be rejected");
    println!("[strictness] Unknown field rejected: {error}");

    println!(
        "\nResult: the validated manifest is ready for a create or apply boundary; the state store has not materialized it."
    );
    Ok(())
}
