//! # solti-api - task management API.
//!
//! Dual-transport API layer exposing task operations over gRPC and HTTP.
//! Both transports share the same wire types generated from `proto/solti/task/v1/*.proto` and delegate to an [`ApiHandler`] implementation.
//!
//! | feature        | capability                                      |
//! |----------------|-------------------------------------------------|
//! | `core-adapter` | `SupervisorApiAdapter` for `solti-core`         |
//! | `grpc`         | tonic gRPC server                               |
//! | `grpc-tls`     | gRPC TLS adapter; implies `grpc`                |
//! | `http`         | axum HTTP/JSON server                           |
//!
//! ## Quick start
//!
//! Build one [`ApiHandler`] and share it across both transports.
//! The handler is `Arc`-wrapped once, then cloned into each server:
//!
#![cfg_attr(
    all(feature = "core-adapter", feature = "grpc", feature = "http"),
    doc = "```rust,no_run"
)]
#![cfg_attr(
    not(all(feature = "core-adapter", feature = "grpc", feature = "http")),
    doc = "```rust,no_run,ignore"
)]
//! # use std::sync::Arc;
//! # use solti_api::{GrpcApi, HttpApi, SupervisorApiAdapter};
//! # fn wire(supervisor: Arc<solti_core::SupervisorApi>) {
//! let handler = Arc::new(SupervisorApiAdapter::new(supervisor));
//! let grpc    = GrpcApi::new(handler.clone()).server();
//! let http    = HttpApi::new(handler).router();
//! # let _ = (grpc, http);
//! # }
//! ```
//!
//! ## Also
//!
//! - [`ApiHandler`] transport-agnostic trait with 7 operations.
//! - [`ApiError`] unified error type mapped to gRPC Status / HTTP JSON.
//! - `SupervisorApiAdapter` optional adapter bridging to `SupervisorApi`
//!   (feature `core-adapter`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
///
/// Gated on every feature: the README examples cover both transports and TLS.
#[cfg(all(
    doctest,
    feature = "core-adapter",
    feature = "grpc",
    feature = "grpc-tls",
    feature = "http"
))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

#[doc(hidden)]
#[macro_export]
macro_rules! solti_api_major {
    () => {
        1
    };
}

/// Compose a compile-time URL path rooted at `/api/v<API_MAJOR>`.
#[cfg(feature = "http")]
#[doc(hidden)]
#[macro_export]
macro_rules! api_url {
    ($path:literal) => {
        concat!("/api/v", $crate::solti_api_major!(), $path)
    };
}

/// Current API protocol version.
pub const API_VERSION: u32 = solti_api_major!();

/// Maximum accepted request body / message size for both HTTP and gRPC transports. **4 MiB.**
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

mod error;
pub use error::ApiError;

mod handler;
pub use handler::{ApiHandler, OutputEventStream};

#[cfg(feature = "core-adapter")]
mod adapter;
#[cfg(feature = "core-adapter")]
pub use adapter::SupervisorApiAdapter;

mod metrics;
#[cfg(feature = "http")]
pub use metrics::http_metrics_middleware;
pub use metrics::{
    ApiMetricsBackend, ApiMetricsHandle, NoOpApiMetrics, Transport, noop_api_metrics,
};

// Generated prost/pbjson output carries no doc comments; suppress the doc
// lints on this module only. Never suppress them crate-wide.
#[cfg(any(feature = "grpc", feature = "http"))]
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
#[allow(missing_docs)]
#[allow(rustdoc::all)]
pub(crate) mod proto_api {
    include!(concat!(
        env!("OUT_DIR"),
        "/solti.task.v",
        solti_api_major!(),
        ".rs"
    ));

    #[cfg(feature = "http")]
    include!(concat!(
        env!("OUT_DIR"),
        "/solti.task.v",
        solti_api_major!(),
        ".serde.rs"
    ));
}

#[cfg(any(feature = "grpc", feature = "http"))]
mod auth;

#[cfg(any(feature = "grpc", feature = "http"))]
mod convert;

#[cfg(any(feature = "grpc", feature = "http"))]
mod validate;

#[cfg(feature = "grpc")]
mod grpc;

#[cfg(feature = "grpc")]
pub use grpc::{BearerAuth, GrpcApi, GrpcServer, TaskApiService};

#[cfg(feature = "grpc")]
pub use proto_api::task_service_server::TaskServiceServer;

#[cfg(feature = "grpc")]
pub use tonic;

#[cfg(feature = "http")]
mod http;

#[cfg(feature = "http")]
pub use http::HttpApi;

#[cfg(feature = "http")]
pub use axum;

#[cfg(feature = "grpc-tls")]
mod tls;

#[cfg(feature = "grpc-tls")]
pub use tls::to_tonic_server_tls;

#[cfg(all(test, any(feature = "grpc", feature = "http")))]
mod api_major_guard {
    #[test]
    fn api_major_matches_build_rs() {
        assert_eq!(
            super::API_VERSION.to_string(),
            env!("SOLTI_API_MAJOR"),
            "lib.rs solti_api_major!() must match build.rs API_MAJOR",
        );
    }
}
