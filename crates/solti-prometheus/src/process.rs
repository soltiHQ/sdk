//! Process-level metrics: standard Prometheus `process_*` collectors.
//!
//! Exposes on Linux (no-op on other targets):
//! - `process_resident_memory_bytes`
//! - `process_virtual_memory_bytes`
//! - `process_start_time_seconds`
//! - `process_cpu_seconds_total`
//! - `process_open_fds`
//! - `process_max_fds`

use prometheus::Registry;

/// Register the default Prometheus process collector.
///
/// On Linux registers the upstream [`prometheus::process_collector::ProcessCollector`].
/// On other targets this is a no-op so callers can wire it unconditionally.
#[cfg(all(feature = "process", target_os = "linux"))]
pub fn register_process_collector(registry: &Registry) -> Result<(), prometheus::Error> {
    let collector = prometheus::process_collector::ProcessCollector::for_self();
    registry.register(Box::new(collector))
}

#[cfg(not(all(feature = "process", target_os = "linux")))]
pub fn register_process_collector(_registry: &Registry) -> Result<(), prometheus::Error> {
    Ok(())
}
