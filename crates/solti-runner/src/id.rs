//! # Run identifier generation
//!
//! [`RunId`] is the per-execution task name for taskvisor.
//!
//! It is formatted as `{runner}-{slot}-{seq}` and is unique per call.
//! The sequence is a process-global counter.
//!
//! A `RunId` is not the admission slot. The slot is stable.
//! The run id names one concrete run instance.

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
    ///
    /// Format: `{runner}-{slot}-{seq}`, unique per submission.
    ///
    /// ## Example
    ///
    /// ```
    /// let id = solti_runner::make_run_id("subprocess", "slot-a");
    /// assert!(id.name().starts_with("subprocess-slot-a-"));
    /// ```
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Raw sequence number (monotonically increasing per process).
    ///
    /// ## Example
    ///
    /// ```
    /// let id = solti_runner::make_run_id("subprocess", "slot-a");
    /// assert!(id.seq() >= 1);
    /// ```
    #[inline]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Consume and return the name as an owned `String`.
    ///
    /// ## Example
    ///
    /// ```
    /// let id = solti_runner::make_run_id("subprocess", "slot-a");
    /// let name = id.into_name();
    ///
    /// assert!(name.starts_with("subprocess-slot-a-"));
    /// ```
    #[inline]
    pub fn into_name(self) -> String {
        self.name
    }
}

/// Build the per-execution run id.
///
/// The taskvisor task name is `{runner}-{slot}-{seq}` and unique per call.
///
/// - `runner` - runner name.
/// - `slot` - logical task slot.
/// - `seq` - per-process counter.
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
    RunId { name, seq }
}

#[cfg(test)]
mod tests {
    use super::make_run_id;

    #[test]
    fn accepts_valid_runner_and_slot_identity_characters() {
        let id = make_run_id("Runner_A", "Build.Step_1");
        assert!(id.name().starts_with("Runner_A-Build.Step_1-"));
    }
}
