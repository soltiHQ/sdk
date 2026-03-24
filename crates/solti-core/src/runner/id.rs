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
    /// Human-readable id used as task name for taskvisor.
    ///
    /// Format: `{runner}-{slot}-{seq}`.
    pub name: String,
    /// Raw sequence number (monotonically increasing per process).
    pub seq: u64,
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
