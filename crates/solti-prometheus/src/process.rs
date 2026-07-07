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
/// Registers the upstream [`prometheus::process_collector::ProcessCollector`]
/// into `registry`. It exposes the standard `process_*` metrics for the
/// current process.
///
/// ## Errors
///
/// - [`prometheus::Error::AlreadyReg`]: the process collector is already registered in `registry`.
#[cfg(all(feature = "process", target_os = "linux"))]
pub fn register_process_collector(registry: &Registry) -> Result<(), prometheus::Error> {
    let collector = prometheus::process_collector::ProcessCollector::for_self();
    registry.register(Box::new(collector))
}

/// Register the default Prometheus process collector (fallback).
///
/// The upstream process collector only works on Linux. This fallback is
/// compiled on other targets and when the `process` feature is off. It does
/// nothing and returns `Ok(())`. Callers can wire it unconditionally.
///
/// ## Errors
///
/// This fallback never returns an error.
#[cfg(not(all(feature = "process", target_os = "linux")))]
pub fn register_process_collector(_registry: &Registry) -> Result<(), prometheus::Error> {
    Ok(())
}
