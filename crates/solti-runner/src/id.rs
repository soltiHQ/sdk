//! # Run identity
//!
//! [`RunId`] identifies one task built through a runner router.
//! It does not identify an individual execution attempt.
//!
//! ## Flow
//!
//! ```text
//! runner + slot + process sequence
//!               ▼
//!       runner-slot-sequence
//!               ▼
//!           BuiltTask
//! ```
//!
//! The slot comes from task desired state.
//! The process sequence distinguishes separate allocations.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-local sequence for run identifiers.
///
/// The first returned value is `1`.
/// Zero is the exhausted sentinel and is never returned.
static RUN_SEQ: AtomicU64 = AtomicU64::new(1);

#[inline]
fn advance_seq(current: u64) -> Option<u64> {
    match current {
        0 => None,
        u64::MAX => Some(0),
        value => Some(value + 1),
    }
}

/// Returns the next process-local sequence value.
fn next_seq() -> u64 {
    RUN_SEQ
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, advance_seq)
        .unwrap_or_else(|_| panic!("run id sequence exhausted; identities cannot wrap safely"))
}

/// Identity allocated for one runner build.
///
/// The format is `{runner}-{slot}-{sequence}`.
/// The sequence is local to the current process.
/// It starts at `1` and never wraps.
///
/// The allocator does not persist its counter across process restarts.
/// It is unique within one process.
///
/// ## Example
///
/// ```
/// let id = solti_runner::make_run_id("subprocess", "slot-a");
///
/// assert!(id.name().starts_with("subprocess-slot-a-"));
/// assert!(id.seq() >= 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunId {
    name: String,
    seq: u64,
}

impl RunId {
    /// Returns the allocated run name.
    ///
    /// Use this name when constructing the surrounding `taskvisor::TaskSpec`.
    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the raw process-local sequence value.
    #[inline]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Consumes the id and returns its name.
    #[inline]
    pub fn into_name(self) -> String {
        self.name
    }
}

/// Builds a run id from a runner name and task slot.
///
/// This function joins both values with the next process sequence.
/// It does not validate either input.
///
/// # Panics
///
/// Panics after the process-local sequence space is exhausted. This preserves
/// identity uniqueness instead of wrapping into an earlier allocation.
///
/// ## Example
///
/// ```rust
/// use solti_runner::make_run_id;
///
/// let id = make_run_id("subprocess", "my-slot");
/// assert!(id.name().starts_with("subprocess-my-slot-"));
/// assert!(id.seq() >= 1);
/// ```
pub fn make_run_id(runner_name: &str, slot: &str) -> RunId {
    let seq = next_seq();
    let name = format!("{runner_name}-{slot}-{seq}");
    RunId { name, seq }
}

#[cfg(test)]
mod tests {
    use super::{advance_seq, make_run_id};

    #[test]
    fn sequence_uses_zero_as_an_exhausted_sentinel() {
        assert_eq!(advance_seq(1), Some(2));
        assert_eq!(advance_seq(u64::MAX - 1), Some(u64::MAX));
        assert_eq!(advance_seq(u64::MAX), Some(0));
        assert_eq!(advance_seq(0), None);
    }

    #[test]
    fn run_id_preserves_identity_and_exposes_its_sequence() {
        let first = make_run_id("Runner_A", "Build.Step_1");
        let second = make_run_id("Runner_A", "Build.Step_1");

        assert_eq!(
            first.name(),
            format!("Runner_A-Build.Step_1-{}", first.seq())
        );
        assert!(second.seq() > first.seq());
        assert_eq!(
            second.clone().into_name(),
            format!("Runner_A-Build.Step_1-{}", second.seq())
        );
    }
}
