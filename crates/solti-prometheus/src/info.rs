//! # Build information
//!
//! [`register_build_info`] exposes build identity as one gauge.
//! The metric is always available.
//!
//! ## Flow
//!
//! ```text
//! constant labels ──► solti_build_info = 1 ──► Registry
//! ```

use prometheus::{IntGauge, Opts, Registry};

/// Registers a `solti_build_info{labels...}` gauge with value `1`.
///
/// Labels are stored as constant descriptor labels.
/// The registered value does not change.
///
/// Labels can include `version`, `git_sha`, `rustc`, and `built_at`.
///
/// ## Example
///
/// ```rust
/// use solti_prometheus::{Registry, register_build_info};
///
/// # fn main() -> Result<(), solti_prometheus::Error> {
/// let registry = Registry::new();
/// register_build_info(&registry, &[
///     ("version", env!("CARGO_PKG_VERSION")),
///     ("rustc", "1.90.0"),
/// ])?;
/// # Ok(())
/// # }
/// ```
///
/// # Errors
///
/// Returns a Prometheus error when a label or descriptor is invalid.
/// A descriptor conflict returns [`prometheus::Error::AlreadyReg`].
pub fn register_build_info(
    registry: &Registry,
    labels: &[(&str, &str)],
) -> Result<(), prometheus::Error> {
    let mut opts =
        Opts::new("build_info", "Build-time identity (value always 1).").namespace("solti");
    for (k, v) in labels {
        opts = opts.const_label((*k).to_string(), (*v).to_string());
    }
    let gauge = IntGauge::with_opts(opts)?;
    gauge.set(1);
    registry.register(Box::new(gauge))?;
    Ok(())
}
