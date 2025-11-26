use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use taskvisor::TaskRef;
use tno_core::{BuildContext, Runner, RunnerError};
use tno_model::{CreateSpec, TaskKind};
use tracing::{debug, trace};

use crate::error::ExecError;

/// Runner for pre-registered functions (TaskKind::Fn).
///
/// Functions must be registered before they can be used in task specs.
/// The runner looks up functions by their slot name.
pub struct FnRunner {
    name: &'static str,
    registry: Arc<RwLock<HashMap<String, TaskRef>>>,
}

// impl FnRunner {
//     pub fn new() -> Self {
//         Self {
//             name: "fn",
//             registry: Arc::new(RwLock::new(HashMap::new())),
//         }
//     }
//
//     pub fn with_name(name: &'static str) -> Self {
//         Self {
//             name,
//             registry: Arc::new(RwLock::new(HashMap::new())),
//         }
//     }
//
//     /// Register a function that can be referenced by slot name in task specs.
//     pub fn register(&self, slot: impl Into<String>, task: TaskRef) -> &Self {
//         let slot = slot.into();
//         let mut registry = self.registry.write().unwrap();
//         registry.insert(slot.clone(), task);
//         trace!(slot, "function registered");
//         self
//     }
//
//     /// Unregister a function by slot name.
//     pub fn unregister(&self, slot: &str) -> bool {
//         let mut registry = self.registry.write().unwrap();
//         registry.remove(slot).is_some()
//     }
//
//     /// Check if a function is registered for the given slot.
//     pub fn is_registered(&self, slot: &str) -> bool {
//         let registry = self.registry.read().unwrap();
//         registry.contains_key(slot)
//     }
//
//     /// Get the number of registered functions.
//     pub fn count(&self) -> usize {
//         let registry = self.registry.read().unwrap();
//         registry.len()
//     }
// }
//
// impl Default for FnRunner {
//     fn default() -> Self {
//         Self::new()
//     }
// }
//
// impl Runner for FnRunner {
//     fn name(&self) -> &'static str {
//         self.name
//     }
//
//     fn supports(&self, spec: &CreateSpec) -> bool {
//         matches!(spec.kind, TaskKind::Fn)
//     }
//
//     fn build_task(&self, spec: &CreateSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
//         if !matches!(spec.kind, TaskKind::Fn) {
//             return Err(RunnerError::InvalidSpec(format!(
//                 "expected TaskKind::Fn, got {}",
//                 spec.kind.kind()
//             )));
//         }
//
//         let registry = self.registry.read().unwrap();
//         let task = registry
//             .get(&spec.slot)
//             .ok_or_else(|| {
//                 RunnerError::InvalidSpec(
//                     ExecError::FunctionNotFound(spec.slot.clone()).to_string(),
//                 )
//             })?
//             .clone();
//
//         debug!(slot = %spec.slot, "function resolved from registry");
//         Ok(task)
//     }
// }
//
// #[cfg(test)]
// mod tests {
//     use super::*;
//     use std::sync::Arc;
//     use taskvisor::TaskFn;
//     use tno_model::{AdmissionStrategy, BackoffStrategy, JitterStrategy, RestartStrategy};
//
//     fn create_test_spec(slot: &str) -> CreateSpec {
//         CreateSpec {
//             slot: slot.to_string(),
//             kind: TaskKind::Fn,
//             timeout_ms: 10000,
//             restart: RestartStrategy::Never,
//             backoff: BackoffStrategy {
//                 first_ms: 1000,
//                 max_ms: 60000,
//                 factor: 2.0,
//                 jitter: JitterStrategy::Full,
//             },
//             admission: AdmissionStrategy::DropIfRunning,
//         }
//     }
//
//     fn create_test_task(name: &'static str) -> TaskRef {
//         TaskFn::arc(name, |_ctx| async move { Ok(()) })
//     }
//
//     #[test]
//     fn fn_runner_supports_fn_kind() {
//         let runner = FnRunner::new();
//         let spec = create_test_spec("test");
//         assert!(runner.supports(&spec));
//     }
//
//     #[test]
//     fn fn_runner_does_not_support_other_kinds() {
//         let runner = FnRunner::new();
//         let spec = CreateSpec {
//             slot: "test".to_string(),
//             kind: TaskKind::Exec {
//                 command: "ls".to_string(),
//                 args: vec![],
//                 env: Default::default(),
//                 cwd: None,
//                 fail_on_non_zero: Default::default(),
//             },
//             timeout_ms: 10000,
//             restart: RestartStrategy::Never,
//             backoff: BackoffStrategy {
//                 first_ms: 1000,
//                 max_ms: 60000,
//                 factor: 2.0,
//                 jitter: JitterStrategy::Full,
//             },
//             admission: AdmissionStrategy::DropIfRunning,
//         };
//         assert!(!runner.supports(&spec));
//     }
//
//     #[test]
//     fn register_and_build_task() {
//         let runner = FnRunner::new();
//         let task = create_test_task("test-fn");
//
//         runner.register("my-slot", task);
//
//         let spec = create_test_spec("my-slot");
//         let ctx = BuildContext::default();
//         let result = runner.build_task(&spec, &ctx);
//
//         assert!(result.is_ok());
//     }
//
//     #[test]
//     fn build_task_fails_for_unregistered_function() {
//         let runner = FnRunner::new();
//         let spec = create_test_spec("not-registered");
//         let ctx = BuildContext::default();
//
//         let result = runner.build_task(&spec, &ctx);
//         assert!(matches!(result, Err(RunnerError::InvalidSpec(_))));
//     }
//
//     #[test]
//     fn unregister_removes_function() {
//         let runner = FnRunner::new();
//         let task = create_test_task("test-fn");
//
//         runner.register("test", task);
//         assert!(runner.is_registered("test"));
//         assert_eq!(runner.count(), 1);
//
//         let removed = runner.unregister("test");
//         assert!(removed);
//         assert!(!runner.is_registered("test"));
//         assert_eq!(runner.count(), 0);
//     }
//
//     #[test]
//     fn register_replaces_existing() {
//         let runner = FnRunner::new();
//         let task1 = create_test_task("task1");
//         let task2 = create_test_task("task2");
//
//         runner.register("slot", task1);
//         assert_eq!(runner.count(), 1);
//
//         runner.register("slot", task2);
//         assert_eq!(runner.count(), 1);
//     }
//
//     #[test]
//     fn with_name_sets_custom_name() {
//         let runner = FnRunner::with_name("custom-fn");
//         assert_eq!(runner.name(), "custom-fn");
//     }
// }
