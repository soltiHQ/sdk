use std::sync::atomic::{AtomicU64, Ordering};

/// Global monotonically increasing sequence for run identifiers.
///
/// Local to the current agent process.
static RUN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Returns next numeric sequence value.
fn next_seq() -> u64 {
    RUN_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Build a human-readable run id used as task name for taskvisor.
///
/// Format: `{runner}-{slot}-{seq:x}`.
/// - `runner` — Runner::name()
/// - `slot`   — CreateSpec.slot
/// - `seq`    — per-process hex sequence
pub fn make_run_id(runner_name: &str, slot: &str) -> String {
    format!("{runner_name}-{slot}-{seq:x}", seq = next_seq())
}
