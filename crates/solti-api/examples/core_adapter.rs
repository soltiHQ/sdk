//! # Core adapter visibility
//!
//! `SupervisorApiAdapter` connects the transport-independent API boundary to core.
//! It keeps in-process `Embedded` workloads outside the public task contract.
//!
//! This example shows:
//!
//! - one embedded resource stored in `solti-core`;
//! - direct core visibility;
//! - API get and list filtering;
//! - rejection of embedded create and delete operations;
//! - one shared `ApiHandler` boundary used by HTTP and gRPC.
//!
//! Run with `cargo run -p solti-api --example core_adapter --features core-adapter`.

use std::sync::Arc;

use solti_api::{ApiError, ApiHandler, SupervisorApiAdapter};
use solti_core::SupervisorApi;
use solti_model::{
    EmbeddedSpec, TaskId, TaskManifest, TaskQuery, TaskSpec, TaskWorkload, WritePreconditions,
};
use solti_runner::RunnerRouter;
use taskvisor::{TaskContext, TaskError, TaskFn};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-api: SupervisorApiAdapter visibility boundary

  application-owned TaskRef + Embedded manifest
                         │ create_embedded_task
                         ▼
                    solti-core state
             ┌───────────┴────────────┐
             ▼                        ▼
       direct core read        SupervisorApiAdapter
       Embedded is visible             │ public workload guard
                                       ▼
                              get/list/create/delete
                              Embedded is hidden

  Core keeps the complete SDK model.
  The API adapter exposes only workloads with a public wire representation.
"#;

fn embedded_manifest(name: &str) -> ExampleResult<TaskManifest> {
    let workload = TaskWorkload::Embedded(EmbeddedSpec::new("application-code@1")?);
    let spec = TaskSpec::builder("internal-maintenance", workload, 30_000_u64).build()?;
    Ok(TaskManifest::new(name, spec)?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Verify that the bundled adapter preserves the public wire boundary without removing Embedded from core."
    );

    let supervisor = Arc::new(SupervisorApi::builder(RunnerRouter::new()).start().await?);
    let task_ref = TaskFn::arc(|ctx: TaskContext| async move {
        ctx.cancelled().await;
        Err::<(), TaskError>(TaskError::Canceled)
    });
    let committed = supervisor
        .create_embedded_task(embedded_manifest("internal-cleanup")?, task_ref)
        .await?;
    let name = committed.name().clone();
    println!(
        "[core] Stored task={} workload={}/{}.",
        name,
        committed.spec().workload().api_version(),
        committed.spec().workload().kind(),
    );
    assert!(supervisor.get_task(&name).is_some());

    let adapter = SupervisorApiAdapter::new(Arc::clone(&supervisor));
    let public_get = adapter.get_task(&name).await?;
    let public_page = adapter.query_tasks(TaskQuery::new()).await?;
    println!(
        "[adapter] get_task returned {}; query_tasks returned {} public resources.",
        if public_get.is_some() { "Some" } else { "None" },
        public_page.items.len(),
    );
    assert!(public_get.is_none());
    assert!(public_page.items.is_empty());

    let create_error = adapter
        .create_task(embedded_manifest("wire-create")?)
        .await
        .expect_err("public create must reject Embedded");
    println!(
        "[adapter] Embedded create rejected as {}: {create_error}.",
        create_error.as_label(),
    );
    assert!(matches!(create_error, ApiError::InvalidRequest(_)));

    let delete_error = adapter
        .delete_task(&name, WritePreconditions::new())
        .await
        .expect_err("public delete must not address Embedded");
    println!(
        "[adapter] Embedded delete reported {} and left the core resource intact.",
        delete_error.as_label(),
    );
    assert!(matches!(delete_error, ApiError::TaskNotFound(_)));
    assert!(
        supervisor
            .get_task(&TaskId::new("internal-cleanup")?)
            .is_some()
    );

    supervisor.shutdown().await?;
    println!(
        "\nResult: Embedded stayed available to in-process SDK code and remained absent from every adapter operation."
    );
    Ok(())
}
