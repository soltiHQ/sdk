//! Environment variable types for tasks and runners.
//!
//! ```text
//!  TaskEnv (user-defined)       RunnerEnv (runner-level)
//!  ├─ FOO=from-task             ├─ FOO=from-runner      ← runner wins
//!  ├─ BAR=task-only             ├─ PATH=/safe/bin
//!  └─ BAZ=task                  └─ RUNNER=prod-01
//!            │                           │
//!            └──────────┬────────────────┘
//!                       ▼
//!              merge(task, runner)
//!                       │
//!                       ▼
//!         Vec<(String, String)>  → Command::envs()
//!         ├─ FOO=from-runner       (runner wins)
//!         ├─ BAR=task-only
//!         ├─ BAZ=task
//!         ├─ PATH=/safe/bin
//!         └─ RUNNER=prod-01
//! ```

mod runner;
pub use runner::RunnerEnv;

mod task;
pub use task::TaskEnv;

use std::collections::BTreeMap;

/// Merge task and runner environments.
///
/// **Runner environment takes precedence**: if a key exists in both, the runner value wins.
/// Returns a sorted, deduplicated `BTreeMap` ready for `Command::envs()`.
pub fn merge(task: &TaskEnv, runner: &RunnerEnv) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();

    for kv in task.iter() {
        map.insert(kv.key().to_owned(), kv.value().to_owned());
    }

    for kv in runner.iter() {
        map.insert(kv.key().to_owned(), kv.value().to_owned());
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
}
