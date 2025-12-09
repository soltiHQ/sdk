mod kv;
pub use kv::KeyValue;

mod task_env;
pub use task_env::TaskEnv;

mod flag;
pub use flag::Flag;

mod runner_labels;
pub use runner_labels::RunnerLabels;

mod constants;
pub use constants::LABEL_RUNNER_TAG;

mod task_id;
pub use task_id::TaskId;

mod task_info;
pub use task_info::TaskInfo;

mod task_status;
pub use task_status::TaskStatus;

/// Logical identifier for a controller slot.
///
/// A slot groups tasks that must not run concurrently.
/// The controller enforces admission policies per slot.
pub type Slot = String;

/// Timeout value in milliseconds.
///
/// Used in task specifications and controller rules where an explicit time limit is required.
pub type TimeoutMs = u64;
