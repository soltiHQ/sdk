//! Build-time identity exposed as `solti_build_info` gauge (value always `1`).
//!
//! Labels carry the identity fields — dashboards and alerts use them to pin
//! observations to a specific build. Typical labels:
//!
//!  - `version`
//!  - `git_sha`
//!  - `rustc`
//!  - `built_at`

use prometheus::{IntGauge, Opts, Registry};

/// Register a `solti_build_info{labels...}` gauge with value `1`.
///
/// Labels are set as *constant* labels: they live on the metric descriptor and never change during process lifetime.
///
/// ## Example
///
/// ```text
/// solti_prometheus::register_build_info(&registry, &[
///     ("version", env!("CARGO_PKG_VERSION")),
///     ("rustc",   "1.90.0"),
/// ])?;
/// ```
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
