//! Environment variable types for tasks and runners.
//!
//! ```text
//! TaskEnv + RunnerEnv -> merge_env() -> sorted map for process env
//!
//! If both sides define the same key, RunnerEnv wins.
//! ```

// `#[macro_use]` must come before any module that uses the macro
// (Rust resolves `macro_rules!` top-to-bottom within a module).
#[macro_use]
mod macros;

mod runner;
pub use runner::RunnerEnv;

mod task;
pub use task::TaskEnv;

use std::collections::BTreeMap;

/// Merge task and runner environments.
///
/// Runner environment takes precedence: if a key exists in both, the runner
/// value wins. The result is a sorted, deduplicated map ready for
/// `Command::envs()`.
///
/// ## Example
///
/// ```
/// use solti_model::{RunnerEnv, TaskEnv, merge_env};
///
/// let mut task = TaskEnv::new();
/// task.push("PATH", "/user/bin");
/// task.push("APP_MODE", "batch");
///
/// let mut runner = RunnerEnv::new();
/// runner.push("PATH", "/safe/bin");
///
/// let env = merge_env(&task, &runner);
/// assert_eq!(env.get("PATH").map(String::as_str), Some("/safe/bin"));
/// assert_eq!(env.get("APP_MODE").map(String::as_str), Some("batch"));
/// ```
pub fn merge(task: &TaskEnv, runner: &RunnerEnv) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for kv in runner.into_iter().rev().chain(task.into_iter().rev()) {
        map.entry(kv.key().to_owned())
            .or_insert_with(|| kv.value().to_owned());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_overrides_task() {
        let mut task = TaskEnv::new();
        task.push("FOO", "from-task");
        task.push("BAR", "task-only");

        let mut runner = RunnerEnv::new();
        runner.push("FOO", "from-runner");
        runner.push("PATH", "/safe/bin");

        let result = merge(&task, &runner);

        assert_eq!(result.get("FOO").map(|s| s.as_str()), Some("from-runner"));
        assert_eq!(result.get("BAR").map(|s| s.as_str()), Some("task-only"));
        assert_eq!(result.get("PATH").map(|s| s.as_str()), Some("/safe/bin"));
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn empty_runner_keeps_task_env() {
        let mut task = TaskEnv::new();
        task.push("A", "1");
        task.push("B", "2");

        let runner = RunnerEnv::new();
        let result = merge(&task, &runner);

        assert_eq!(result.len(), 2);
        assert_eq!(result["A"], "1");
        assert_eq!(result["B"], "2");
    }

    #[test]
    fn empty_task_keeps_runner_env() {
        let task = TaskEnv::new();
        let mut runner = RunnerEnv::new();
        runner.push("X", "Y");

        let result = merge(&task, &runner);
        assert_eq!(result.len(), 1);
        assert_eq!(result["X"], "Y");
    }

    #[test]
    fn both_empty_returns_empty() {
        let result = merge(&TaskEnv::new(), &RunnerEnv::new());
        assert!(result.is_empty());
    }

    #[test]
    fn task_duplicate_keys_last_wins_before_merge() {
        let mut task = TaskEnv::new();
        task.push("FOO", "first");
        task.push("FOO", "second");

        let runner = RunnerEnv::new();
        let result = merge(&task, &runner);

        assert_eq!(result["FOO"], "second");
    }

    #[test]
    fn runner_duplicate_keys_last_wins_before_merge() {
        let task = TaskEnv::new();
        let mut runner = RunnerEnv::new();
        runner.push("FOO", "first");
        runner.push("FOO", "second");

        let result = merge(&task, &runner);

        assert_eq!(result["FOO"], "second");
    }

    #[test]
    fn runner_last_value_beats_task_last_value() {
        let mut task = TaskEnv::new();
        task.push("FOO", "task-1");
        task.push("FOO", "task-2");

        let mut runner = RunnerEnv::new();
        runner.push("FOO", "runner-1");
        runner.push("FOO", "runner-2");

        let result = merge(&task, &runner);

        assert_eq!(result["FOO"], "runner-2");
    }
}
