//! # solti-api
//!
//! Public task transports for a Solti agent.
//!
//! HTTP uses the model-owned CRD JSON representation.
//! gRPC uses versioned protobuf messages.
//! Both transports delegate domain operations to one [`ApiHandler`].
//!
//! This crate does not store or execute tasks.
//!
//! ## Start Here
//!
//! Use [`ApiHandler`] to define the transport-independent backend.
//! Use `SupervisorApiAdapter` to connect that boundary to `solti-core`.
//! Use `HttpApi` to build an axum router.
//! Use `GrpcApi` to build a tonic service.
//!
//! ## Flow
//!
//! ```text
//! HTTP CRD JSON ── parse and validate ──┐
//!                                       ▼
//!                                  ApiHandler
//!                                       ▲
//! gRPC v1 DTO ── convert and validate ──┘
//!                                       │
//!                                       └──► custom backend or solti-core
//! ```
//!
//! The transports own wire validation, authentication, metrics, and error mapping.
//! The handler owns task operations.
//!
//! ## Desired State
//!
//! The bundled adapter commits desired state before reconciliation finishes.
//! A successful create or apply does not mean that execution has started.
//! Clients observe reconciliation through `status.conditions[type=Reconciled]`.
//!
//! Apply is an upsert without write preconditions.
//! Apply and delete can check `uid` and `resourceVersion`.
//!
//! ## Collections and Streams
//!
//! Lists use opaque continuation tokens.
//! The bundled adapter provides snapshot-consistent pagination.
//! Watches can resume from a retained resource version.
//!
//! Task output is live-only and lossy.
//! It is not persisted or replayed.
//! A slow subscriber receives a `Lagged` event.
//!
//! ## Workload Boundary
//!
//! The built-in `Embedded` workload is available only through the in-process SDK.
//! HTTP and gRPC reject it.
//! Extension workloads remain visible.
//!
//! ## Feature Flags
//!
//! | Feature        | Capability                                      |
//! |----------------|-------------------------------------------------|
//! | `core-adapter` | `SupervisorApiAdapter` for `solti-core`         |
//! | `grpc`         | tonic gRPC service and generated v1 client      |
//! | `grpc-tls`     | `solti-tls` adapter for tonic; implies `grpc`   |
//! | `http`         | axum HTTP/JSON router                           |
//!
//! No feature is enabled by default.
//!
//! ## Main Types
//!
//! | Area          | Types                                                   |
//! |---------------|---------------------------------------------------------|
//! | Handler       | [`ApiHandler`], [`ApiError`]                             |
//! | Streams       | [`TaskWatchEventStream`], [`OutputEventStream`]          |
//! | Metrics       | [`ApiMetricsBackend`], [`ApiMetricsHandle`], [`Transport`] |
//! | HTTP          | `HttpApi`                                               |
//! | gRPC          | `GrpcApi`, `grpc::v1`                                   |
//! | Core adapter  | `SupervisorApiAdapter`                                  |
//!
//! ## Quick Start
//!
//! Build both transports from one handler:
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
//! let grpc = GrpcApi::new(handler.clone()).server();
//! let http = HttpApi::new(handler).router();
//! # let _ = (grpc, http);
//! # }
//! ```

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

/// Compose a compile-time Kubernetes named-group URL rooted at
/// `/apis/solti.io/v<API_MAJOR>`.
#[cfg(feature = "http")]
#[doc(hidden)]
#[macro_export]
macro_rules! api_url {
    ($path:literal) => {
        concat!("/apis/solti.io/v", $crate::solti_api_major!(), $path)
    };
}

/// Current public API major version.
pub const API_VERSION: u32 = solti_api_major!();

/// Root path of the HTTP Kubernetes API group.
#[cfg(feature = "http")]
pub const HTTP_API_ROOT: &str = api_url!("");

/// Maximum HTTP request body and gRPC message size.
///
/// The limit is 4 MiB.
pub const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

mod error;
pub use error::{ApiConflict, ApiError, ApiErrorCause};

mod handler;
pub use handler::{ApiHandler, OutputEventStream, TaskWatchEventStream};

#[cfg(any(feature = "grpc", feature = "http"))]
mod continuation;

#[cfg(feature = "core-adapter")]
mod adapter;
#[cfg(feature = "core-adapter")]
pub use adapter::SupervisorApiAdapter;

mod metrics;
pub use metrics::{
    ApiMetricsBackend, ApiMetricsHandle, NoOpApiMetrics, Transport, noop_api_metrics,
};

// Generated prost output carries no doc comments; suppress the doc
// lints on this module only. Never suppress them crate-wide.
#[cfg(feature = "grpc")]
#[allow(missing_docs)]
#[allow(rustdoc::all)]
pub(crate) mod proto_api {
    include!(concat!(
        env!("OUT_DIR"),
        "/solti.task.v",
        solti_api_major!(),
        ".rs"
    ));
}

#[cfg(any(feature = "grpc", feature = "http"))]
mod auth;

#[cfg(any(feature = "grpc", feature = "http"))]
mod validate;

#[cfg(any(feature = "grpc", feature = "http", feature = "core-adapter"))]
mod visibility;

#[cfg(feature = "grpc")]
pub mod grpc;

#[cfg(feature = "grpc")]
pub use grpc::GrpcApi;

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

    #[cfg(feature = "http")]
    #[test]
    fn http_api_root_uses_the_current_named_group_version() {
        assert_eq!(super::HTTP_API_ROOT, "/apis/solti.io/v1");
        assert_eq!(super::api_url!("/tasks"), "/apis/solti.io/v1/tasks");
        assert_eq!(
            super::HTTP_API_ROOT.strip_prefix("/apis/"),
            Some(solti_model::TASK_API_VERSION),
            "HTTP named group must match the Task resource apiVersion",
        );
    }
}
