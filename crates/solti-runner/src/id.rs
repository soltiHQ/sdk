//! # Run identifier generation.
//!
//! [`RunId`] is a human-readable task name for taskvisor, formatted as `{runner}-{slot}-{seq}`.
//! The sequence is process-global and monotonically increasing (starts at 1).
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
    /// Human-readable id used as task name for taskvisor.
    ///
    /// Format: `{runner}-{slot}-{seq}`.
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

/// Build a human-readable run id used as task name for taskvisor.
///
/// Format: `{runner}-{slot}-{seq}`.
/// - `runner` - Runner::name()
/// - `slot`   - TaskSpec.slot
/// - `seq`    - per-process decimal sequence
///
/// Returns both the formatted name and the raw sequence number,
/// so callers that need the seq (e.g. for cgroup naming) don't
/// have to parse it back out of the string.
pub fn make_run_id(runner_name: &str, slot: &str) -> RunId {
    let seq = next_seq();
    RunId {
        name: format!("{runner_name}-{slot}-{seq}"),
        seq,
    }
}
