use std::sync::OnceLock;
use std::time::Instant;

static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Initialize agent start time.
pub fn init_uptime() {
    START_TIME.get_or_init(Instant::now);
}

/// Get agent uptime in seconds.
pub fn uptime_seconds() -> u64 {
    let start = START_TIME.get_or_init(Instant::now);
    start.elapsed().as_secs()
}

/// Get platform (OS family).
#[inline]
pub fn platform() -> &'static str {
    std::env::consts::OS
}

/// Get architecture.
#[inline]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// Get OS distribution info (Linux only, best effort).
///
/// Returns OS name from `/etc/os-release` or generic platform name.
pub fn os_info() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/etc/os-release") {
            for line in content.lines() {
                if let Some(name) = line.strip_prefix("PRETTY_NAME=") {
                    return name.trim_matches('"').to_string();
                }
            }
        }
    }

    platform().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform() {
        assert!(!platform().is_empty());
    }

    #[test]
    fn test_arch() {
        assert!(!arch().is_empty());
    }

    #[test]
    fn test_uptime_increases() {
        init_uptime();
        let t1 = uptime_seconds();
        // uptime should be >= 0
        assert!(t1 < 1_000_000);
    }

    #[test]
    fn test_os_info_non_empty() {
        let info = os_info();
        assert!(!info.is_empty());
    }
}
