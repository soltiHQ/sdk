//! # Run identifier generation.
//!
//! [`RunId`] is the per-execution task name for taskvisor, formatted as `{runner}-{slot}-{seq}` and **unique per submission** (the sequence is a process-global counter).
//! Uniqueness is intentional: the name identifies one *run instance* (used for events, logs, cgroup naming and per-task state tracking).
//!
//! It is NOT the admission slot.
//! The stable slot is set separately via [`TaskSpec::with_slot`](taskvisor::TaskSpec::with_slot).
//!
//! See [`Runner::build_run_id`](crate::Runner::build_run_id) for the default id builder.

use std::sync::atomic::{AtomicU64, Ordering};

/// Global monotonically increasing sequence for run identifiers.
///
/// Local to the current agent process.
static RUN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Returns next numeric sequence value.
fn next_seq() -> u64 {
    RUN_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Result of [`make_run_id`]: a human-readable run id and the raw sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunId {
    name: String,
    seq: u64,
}

impl RunId {
    /// Per-execution task name for taskvisor.
    /// Format: `{runner}-{slot}-{seq}`, unique per submission.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Raw sequence number (monotonically increasing per process).
    #[inline]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Consume and return the name as an owned `String`.
    #[inline]
    pub fn into_name(self) -> String {
        self.name
    }
}

/// Build the per-execution run id.
///
/// The taskvisor task name is `{runner}-{slot}-{seq}` and **unique per call** — it identifies one run instance.
/// - `runner` - Runner::name()
/// - `slot`   - TaskSpec.slot (logical)
/// - `seq`    - per-process counter (also used verbatim for cgroup naming)
///
/// ## Example
///
/// ```rust
/// use solti_runner::make_run_id;
///
/// let id = make_run_id("subprocess", "my-slot");
/// assert!(id.name().starts_with("subprocess-my-slot-"));
/// assert!(id.seq() >= 1); // process-global counter starts at 1
/// ```
pub fn make_run_id(runner_name: &str, slot: &str) -> RunId {
    let seq = next_seq();
    let name = format!("{runner_name}-{slot}-{seq}");
    debug_assert!(
        solti_model::TaskId::from(name.as_str())
            .validate_format()
            .is_ok(),
        "make_run_id produced an invalid identity {name:?}: runner/slot has illegal chars or is too long"
    );
    RunId { name, seq }
}
