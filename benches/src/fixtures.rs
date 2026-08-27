//! Runtime construction and bounded observation for process benchmarks.

use std::{future::Future, time::Duration};

use solti_core::SupervisorApi;
use solti_model::{
    AdmissionPolicy, EmbeddedSpec, RestartPolicy, Task, TaskFilter, TaskId, TaskManifest, TaskSpec,
    TaskWorkload,
};
use tokio::runtime::Runtime;
use tokio_stream::StreamExt;

/// Failure bound, not a synchronization delay or measured service objective.
pub const WAIT_BOUND: Duration = Duration::from_secs(30);

/// A named runtime constructor used by benchmark variants.
pub type RuntimeVariant = (&'static str, fn() -> Runtime);

/// Tokio runtime names used by the common report.
pub const RUNTIMES: [RuntimeVariant; 2] = [
    ("current_thread", current_thread),
    ("multi_thread", multi_thread),
];

/// Creates a current-thread runtime. Construction belongs outside steady-state timers.
pub fn current_thread() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark Tokio runtime")
}

/// Creates a four-worker runtime, matching the Taskvisor reference benchmarks.
pub fn multi_thread() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("benchmark Tokio runtime")
}

/// Bounds a fixture operation and reports its call site if the deadline expires.
#[track_caller]
pub fn bounded<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let caller = std::panic::Location::caller();
    async move {
        tokio::time::timeout(WAIT_BOUND, future)
            .await
            .unwrap_or_else(|error| {
                panic!("benchmark operation at {caller} exceeded {WAIT_BOUND:?}: {error}")
            })
    }
}

/// Creates a queued Embedded manifest for one non-restarting task.
///
/// Terminal SDK state and cancel/delete acknowledgements are not controller-Idle
/// barriers. Queue allows repeated successful submissions to reuse a slot while
/// its previous owner retires. Policy benchmarks override this choice explicitly.
pub fn embedded_manifest(name: &str, slot: &str) -> TaskManifest {
    let spec = TaskSpec::builder(
        slot,
        TaskWorkload::Embedded(EmbeddedSpec::new("bench-v1").expect("benchmark revision")),
        30_000_u64,
    )
    .admission(AdmissionPolicy::Queue)
    .restart(RestartPolicy::Never)
    .build()
    .expect("benchmark task spec");
    TaskManifest::new(name, spec).expect("benchmark manifest")
}

/// Observes a matching Task state through a watch opened before the current-state check.
///
/// A matching phase is SDK-state visibility, not proof of physical attempt cleanup.
pub async fn wait_task(
    supervisor: &SupervisorApi,
    name: &TaskId,
    predicate: impl Fn(&Task) -> bool,
) -> Task {
    let mut watch = supervisor
        .watch_tasks(&TaskFilter::new(), None)
        .expect("benchmark task watch");
    if let Some(task) = supervisor.get_task(name)
        && predicate(&task)
    {
        return task;
    }
    tokio::time::timeout(WAIT_BOUND, async {
        loop {
            let task = watch
                .next()
                .await
                .expect("benchmark watch closed before the expected state")
                .expect("benchmark watch expired")
                .into_object();
            if task.name() == name && predicate(&task) {
                return task;
            }
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "benchmark task observation exceeded {WAIT_BOUND:?}: {error}; name={name}; current={:?}",
            supervisor.get_task(name)
        )
    })
}

/// Waits for terminal SDK-state visibility for a non-restarting fixture task.
///
/// Retry cases must use [`wait_task`] with an explicit expected attempt and outcome.
pub async fn wait_terminal(supervisor: &SupervisorApi, name: &TaskId) -> Task {
    wait_task(supervisor, name, |task| task.status().phase().is_terminal()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use solti_model::TaskPhase;
    use solti_runner::RunnerRouter;
    use taskvisor::{TaskContext, TaskFn};

    #[test]
    fn observes_terminal_state_without_a_polling_sleep() {
        current_thread().block_on(async {
            let supervisor = SupervisorApi::builder(RunnerRouter::new())
                .start()
                .await
                .unwrap();
            let manifest = embedded_manifest("fixture-observation", "fixture-slot");
            let name = manifest.name().clone();
            supervisor
                .create_embedded_task(manifest, TaskFn::arc(|_: TaskContext| async { Ok(()) }))
                .await
                .unwrap();
            assert_eq!(
                wait_terminal(&supervisor, &name).await.status().phase(),
                TaskPhase::Succeeded
            );
            bounded(supervisor.shutdown()).await.unwrap();
        });
    }
}
