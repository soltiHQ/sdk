//! Process metrics.
//!
//! Registers Prometheus' standard `process_*` collectors when the platform and feature set support them.
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
/// Registers the upstream [`prometheus::process_collector::ProcessCollector`] into `registry`.
/// It exposes the standard `process_*` metrics for the current process.
///
/// # Example
///
/// ```
/// use solti_prometheus::{Registry, register_process_collector};
///
/// # fn main() -> Result<(), prometheus::Error> {
/// let registry = Registry::new();
/// register_process_collector(&registry)?;
/// # Ok(()) }
/// ```
#[cfg(target_os = "linux")]
pub fn register_process_collector(registry: &Registry) -> Result<(), prometheus::Error> {
    let collector = prometheus::process_collector::ProcessCollector::for_self();
    registry.register(Box::new(collector))
}

/// Register the default Prometheus process collector on a non-Linux target.
///
/// The upstream process collector only works on Linux.
/// This implementation does nothing and returns `Ok(())`.
///
/// # Example
///
/// ```
/// use solti_prometheus::{Registry, register_process_collector};
///
/// # fn main() -> Result<(), prometheus::Error> {
/// let registry = Registry::new();
/// register_process_collector(&registry)?;
/// # Ok(()) }
/// ```
#[cfg(not(target_os = "linux"))]
pub fn register_process_collector(_registry: &Registry) -> Result<(), prometheus::Error> {
    Ok(())
}
