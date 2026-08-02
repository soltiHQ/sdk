//! # Task query
//!
//! `TaskFilter` defines collection membership.
//! `TaskQuery` adds list pagination values.
//! A state store executes the query against one retained snapshot.
//!
//! This example shows:
//!
//! - AND semantics across slot, phase, and labels;
//! - OR semantics inside the phase list;
//! - Kubernetes label-selector syntax;
//! - a page limit and domain continuation;
//! - the boundary between model values and state-store execution.
//!
//! Run with `cargo run -p solti-model --example task_query`.

use solti_model::{
    EmbeddedSpec, LabelSelector, Labels, Slot, Task, TaskContinuation, TaskPage, TaskQuery,
    TaskSpec, TaskWorkload,
};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-model: filtered snapshot query

  TaskFilter
      ├── slot ─────────────────────────┐
      ├── phases (OR) ──────────────────┤
      └── label requirements (AND) ─────┤
                                        ├──► TaskQuery
  pagination                            │        ├── limit
      ├── page limit ───────────────────┤        └── continuation
      └── previous continuation ────────┘                │
                                                         ▼
                                                  state store
                                                         │ retained snapshot
                                                         ▼
                                                     TaskPage
                                                         ├── items
                                                         ├── resourceVersion
                                                         └── next continuation

  The model validates and matches query values.
  The state store owns sorting, snapshot retention, and page execution.
"#;

fn labels(entries: &[(&str, &str)]) -> Labels {
    let mut labels = Labels::new();
    for (key, value) in entries {
        labels.insert(*key, *value);
    }
    labels
}

fn task(
    name: &str,
    slot: &str,
    labels: Labels,
    running: bool,
) -> Result<Task, solti_model::ModelError> {
    let spec = TaskSpec::builder(
        slot,
        TaskWorkload::Embedded(EmbeddedSpec::new("query-example-v1")?),
        5_000_u64,
    )
    .build()?;
    let mut task = Task::new(name, spec)?;
    let current_spec = task.spec().clone();
    task.apply_desired(labels, solti_model::Annotations::new(), current_spec, "1")?;
    if running {
        task.transition_starting(1, 1, "2")?;
    }
    Ok(task)
}

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Build a filtered list contract and show what a snapshot-aware state store receives and returns."
    );

    let selector: LabelSelector =
        "environment=production,tier in (frontend,backend),!tainted".parse()?;
    let query = TaskQuery::new()
        .with_slot(Slot::new("build")?)
        .with_active()
        .with_label_selector(selector)?
        .with_limit(2);
    println!("[query] slot=build, phases=Pending|Running, limit=2.");
    println!(
        "[query] labels: environment=production AND tier in (frontend,backend) AND tainted absent."
    );

    let mut collection = vec![
        task(
            "build-a",
            "build",
            labels(&[("environment", "production"), ("tier", "frontend")]),
            false,
        )?,
        task(
            "build-b",
            "build",
            labels(&[("environment", "production"), ("tier", "backend")]),
            true,
        )?,
        task(
            "build-c",
            "build",
            labels(&[
                ("environment", "production"),
                ("tier", "frontend"),
                ("tainted", "true"),
            ]),
            false,
        )?,
        task(
            "build-d",
            "build",
            labels(&[("environment", "production"), ("tier", "frontend")]),
            false,
        )?,
        task(
            "ops-a",
            "ops",
            labels(&[("environment", "production"), ("tier", "frontend")]),
            true,
        )?,
    ];

    for task in &collection {
        println!(
            "[match] name={}, slot={}, phase={} -> {}.",
            task.name(),
            task.slot(),
            task.phase(),
            if query.matches(task) {
                "include"
            } else {
                "exclude"
            },
        );
    }

    collection.retain(|task| query.matches(task));
    collection.sort_by(|left, right| left.name().as_str().cmp(right.name().as_str()));
    assert_eq!(collection.len(), 3);

    let remaining_item_count = collection.len().saturating_sub(query.limit());
    let items: Vec<Task> = collection.into_iter().take(query.limit()).collect();
    let after = items
        .last()
        .ok_or("the first page must contain at least one task")?
        .name()
        .clone();
    let continuation = TaskContinuation::new("store:42", query.filter().clone(), after)?;
    let page = TaskPage {
        items,
        resource_version: "store:42".into(),
        continuation: Some(continuation),
        remaining_item_count,
    };

    println!(
        "[page] resourceVersion={}, items={}, remaining={}.",
        page.resource_version,
        page.items.len(),
        page.remaining_item_count,
    );
    for task in &page.items {
        println!("      {}", task.name());
    }
    let continuation = page
        .continuation
        .as_ref()
        .ok_or("a remaining item requires a continuation")?;
    println!(
        "[continuation] snapshot={}, after={}.",
        continuation.resource_version(),
        continuation.after(),
    );
    println!(
        "[continuation] The original filter is fixed inside the cursor: {}",
        serde_json::to_string(continuation.filter())?,
    );

    println!(
        "\nResult: the query selected three tasks; the first page carries two and resumes after build-b in snapshot store:42."
    );
    Ok(())
}
