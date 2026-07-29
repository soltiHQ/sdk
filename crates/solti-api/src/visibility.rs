//! # Workload Visibility
//!
//! Public transports hide only the built-in `solti.io/v1` `Embedded` GVK.
//! Embedded tasks remain available through the in-process SDK.
//! Every other built-in or extension GVK remains visible.

#[cfg(any(feature = "grpc", feature = "http"))]
use solti_model::TaskRun;
#[cfg(any(feature = "core-adapter", feature = "http"))]
use solti_model::{Task, TaskManifest};
use solti_model::{WORKLOAD_API_VERSION, WorkloadTypeMeta};

const EMBEDDED_KIND: &str = "Embedded";

/// Checks whether a workload GVK can cross a public transport.
pub(crate) fn workload_is_visible(workload: &WorkloadTypeMeta) -> bool {
    workload.api_version() != WORKLOAD_API_VERSION || workload.kind() != EMBEDDED_KIND
}

/// Checks whether a manifest can cross a public transport.
#[cfg(any(feature = "core-adapter", feature = "http"))]
pub(crate) fn manifest_is_visible(manifest: &TaskManifest) -> bool {
    workload_is_visible(&manifest.spec().workload().type_meta())
}

/// Checks whether a stored task can cross a public transport.
#[cfg(any(feature = "core-adapter", feature = "http"))]
pub(crate) fn task_is_visible(task: &Task) -> bool {
    workload_is_visible(&task.spec().workload().type_meta())
}

/// Checks whether a historical run can cross a public transport.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn run_is_visible(run: &TaskRun) -> bool {
    workload_is_visible(run.workload())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_builtin_embedded_gvk_is_hidden() {
        let embedded = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, EMBEDDED_KIND).unwrap();
        let subprocess = WorkloadTypeMeta::new(WORKLOAD_API_VERSION, "Subprocess").unwrap();
        let extension = WorkloadTypeMeta::new("example.io/v1", "Custom").unwrap();

        assert!(!workload_is_visible(&embedded));
        assert!(workload_is_visible(&subprocess));
        assert!(workload_is_visible(&extension));
    }
}
