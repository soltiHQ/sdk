use std::sync::OnceLock;
use std::time::Instant;

static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Initialize agent start time.
pub(crate) fn init_uptime() {
    START_TIME.get_or_init(Instant::now);
}

/// Get agent uptime in seconds.
pub fn uptime_seconds() -> u64 {
    let start = START_TIME.get_or_init(Instant::now);
    start.elapsed().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uptime_increases() {
        init_uptime();
        let t1 = uptime_seconds();
        assert!(t1 < 1_000_000);
    }
}
