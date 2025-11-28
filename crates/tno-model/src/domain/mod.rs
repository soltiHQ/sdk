mod kv;
pub use kv::KeyValue;

mod env;
pub use env::Env;

mod flag;
pub use flag::Flag;

mod labels;
pub use labels::Labels;

mod constants;
pub use constants::LABEL_RUNNER_TAG;

/// Logical identifier for a controller slot.
///
/// A slot groups tasks that must not run concurrently.
/// The controller enforces admission policies per slot.
pub type Slot = String;

/// Timeout value in milliseconds.
///
/// Used in task specifications and controller rules where an explicit time limit is required.
pub type TimeoutMs = u64;
