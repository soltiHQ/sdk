//! # Process metrics
//!
//! [`register_process_collector`] connects Prometheus process metrics to a registry.
//! Enable it with the `process` feature.
//!
//! ## Platform Flow
//!
//! ```text
//! Linux    ──► ProcessCollector::for_self() ──► Registry
//! other OS ──► no-op
//! ```
//!
//! ## Linux Metrics
//!
//! - `process_resident_memory_bytes`
//! - `process_virtual_memory_bytes`
//! - `process_start_time_seconds`
//! - `process_cpu_seconds_total`
//! - `process_open_fds`
//! - `process_max_fds`

use prometheus::Registry;

/// Registers metrics for the current Linux process.
///
/// This uses the upstream [`prometheus::process_collector::ProcessCollector`].
///
/// ## Example
///
/// ```
/// use solti_prometheus::{Registry, register_process_collector};
///
/// # fn main() -> Result<(), solti_prometheus::Error> {
/// let registry = Registry::new();
/// register_process_collector(&registry)?;
/// # Ok(()) }
/// ```
///
/// # Errors
///
/// Returns a Prometheus registration error.
/// Registering another collector with the same descriptors returns [`prometheus::Error::AlreadyReg`].
#[cfg(target_os = "linux")]
pub fn register_process_collector(registry: &Registry) -> Result<(), prometheus::Error> {
    let collector = prometheus::process_collector::ProcessCollector::for_self();
    registry.register(Box::new(collector))
}

/// Accepts process-metric registration on a non-Linux target.
///
/// This function does not register a collector.
/// It always returns `Ok(())`.
///
/// ## Example
///
/// ```
/// use solti_prometheus::{Registry, register_process_collector};
///
/// # fn main() -> Result<(), solti_prometheus::Error> {
/// let registry = Registry::new();
/// register_process_collector(&registry)?;
/// # Ok(()) }
/// ```
#[cfg(not(target_os = "linux"))]
pub fn register_process_collector(_registry: &Registry) -> Result<(), prometheus::Error> {
    Ok(())
}
