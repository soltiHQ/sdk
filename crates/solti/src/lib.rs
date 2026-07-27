//! Thin façade over the modular Solti SDK crates.
//!
//! `solti` contains no runtime logic. Its features select component crates and
//! expose them under owner-preserving namespaces such as [`model`], [`runner`],
//! and [`core`]. All features are disabled by default.
//!
//! ```toml
//! [dependencies]
//! solti = { version = "0.0.3", features = ["core", "exec-subprocess"] }
//! ```
//!
//! ```rust,no_run
//! use solti::core::SupervisorApi;
//! use solti::runner::RunnerRouter;
//!
//! async fn build() -> Result<SupervisorApi, solti::core::CoreError> {
//!     SupervisorApi::builder(RunnerRouter::new()).start().await
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "api")]
pub use solti_api as api;

#[cfg(feature = "core")]
pub use solti_core as core;

#[cfg(feature = "discover")]
pub use solti_discover as discover;

#[cfg(feature = "exec")]
pub use solti_exec as exec;

#[cfg(feature = "model")]
pub use solti_model as model;

#[cfg(feature = "observe")]
pub use solti_observe as observe;

#[cfg(feature = "prometheus-base")]
pub use solti_prometheus as prometheus;

#[cfg(feature = "runner")]
pub use solti_runner as runner;

#[cfg(feature = "taskvisor")]
pub use taskvisor;

#[cfg(feature = "tls")]
pub use solti_tls as tls;
