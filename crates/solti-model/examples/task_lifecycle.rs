//! # Task resource lifecycle
//!
//! A state store materializes a `TaskManifest` as a stored `Task`.
//! Desired-state generation and observed status then evolve independently.
//!
//! This example shows:
//!
//! - initial server-owned metadata and pending status;
//! - reconciliation and execution transitions;
//! - metadata-only apply semantics;
//! - spec apply semantics;
//! - stale-generation event rejection.
//!
//! Run with `cargo run -p solti-model --example task_lifecycle`.

use solti_model::{
    Annotations, ConditionStatus, DesiredChange, EmbeddedSpec, Labels, Task, TaskManifest,
    TaskPhase, TaskSpec, TaskWorkload,
};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-model: desired and observed state

  TaskManifest ──► Task::from_manifest() ──► Task generation 1
                                                   ├──► Pending + Reconciled=Unknown
  controller accepts generation 1 ─────────────────┤
                                                   ├──► Reconciled=True
  execution attempt 1 starts ──────────────────────┤
                                                   ├──► Running
  execution attempt 1 succeeds ────────────────────┤
                                                   └──► Succeeded

  metadata apply ────────────► generation unchanged + status preserved
  spec apply ────────────────► generation 2 + Pending + Reconciled=Unknown
  stale generation 1 event ──► ignored

  resourceVersion changes on accepted stored-resource mutations.
"#;

fn spec(revision: &str) -> Result<TaskSpec, solti_model::ModelError> {
    TaskSpec::builder(
        "maintenance",
        TaskWorkload::Embedded(EmbeddedSpec::new(revision)?),
        5_000_u64,
    )
    .build()
}

fn show(stage: &str, task: &Task) {
    let reconciled = task.status().reconciled();
    println!(
        "[{stage}] rv={}, generation={}, observedGeneration={}, phase={}, attempt={}.",
        task.metadata().resource_version(),
        task.metadata().generation(),
        task.status().observed_generation(),
        task.phase(),
        task.status().attempt(),
    );
    println!(
        "[{stage}] Reconciled={:?}, conditionGeneration={}, reason={}.",
        reconciled.status(),
        reconciled.observed_generation(),
        reconciled.reason(),
    );
}

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Show how Kubernetes-style desired generation and observed status change over time."
    );

    let manifest = TaskManifest::new("daily-cleanup", spec("cleanup-v1")?)?;
    let mut task = Task::from_manifest(manifest)?;
    task.set_resource_version("1")?;
    show("materialized", &task);
    assert_eq!(task.metadata().generation(), 1);
    assert_eq!(task.status().observed_generation(), 0);
    assert_eq!(
        task.status().reconciled().status(),
        ConditionStatus::Unknown
    );

    assert!(task.mark_observed("2")?);
    show("reconciled", &task);
    assert_eq!(task.status().reconciled().status(), ConditionStatus::True);

    assert!(task.transition_starting(1, 1, "3")?);
    show("attempt-started", &task);
    assert_eq!(*task.phase(), TaskPhase::Running);

    assert!(task.transition_finished(1, 1, TaskPhase::Succeeded, None, Some(0), "4")?);
    show("attempt-finished", &task);

    let mut labels = Labels::new();
    labels.insert("environment", "production");
    let current_spec = task.spec().clone();
    let metadata_change = task.apply_desired(labels, Annotations::new(), current_spec, "5")?;
    assert_eq!(metadata_change, DesiredChange::Metadata);
    show("metadata-apply", &task);
    assert_eq!(task.metadata().generation(), 1);
    assert_eq!(*task.phase(), TaskPhase::Succeeded);

    let spec_change = task.apply_desired(
        task.labels().clone(),
        task.metadata().annotations().clone(),
        spec("cleanup-v2")?,
        "6",
    )?;
    assert_eq!(spec_change, DesiredChange::Spec);
    show("spec-apply", &task);
    assert_eq!(task.metadata().generation(), 2);
    assert_eq!(task.status().observed_generation(), 1);
    assert_eq!(
        task.status().reconciled().status(),
        ConditionStatus::Unknown
    );

    let stale_changed = task.transition_starting(1, 2, "7")?;
    assert!(!stale_changed);
    assert_eq!(task.metadata().resource_version(), "6");
    println!("[stale-event] Generation 1 was ignored; resourceVersion remains 6.");

    println!(
        "\nResult: generation tracks desired spec changes; status records only authoritative current-generation observations."
    );
    Ok(())
}
