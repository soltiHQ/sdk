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
//! Use `HttpApi` to build a standalone axum router or mount documented routes
//! into an application router.
//! Use `GrpcApi` to build a tonic service.
//!
//! ## Flow
//!
//! ```text
//! HTTP CRD JSON ── parse and validate ──┐
//!                                       ▼
//!                           authentication + authorization
//!                                       ▼
//!                                  ApiHandler
//!                                       ▲
//! gRPC v1 DTO ── convert and validate ──┘
//!                                       └──► custom backend or solti-core
//! ```
//!
//! The transports own wire validation, access-control hooks, metrics, and error mapping.
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
//! The core adapter maps retained Task count and TaskManifest byte admission
//! failures to [`ApiError::ResourceExhausted`].
//!
//! ## Collections and Streams
//!
//! Lists use opaque continuation tokens.
//! The bundled adapter provides snapshot-consistent pagination.
//! Task list bodies are limited to 4 MiB in each transport's native encoding.
//! TaskRun lists use a separate snapshot and the same native response limit.
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
//! | `grpc`         | tonic gRPC service and generated current client |
//! | `grpc-tls`     | `solti-tls` adapter for tonic; implies `grpc`   |
//! | `http`         | axum HTTP/JSON router                           |
//!
//! No feature is enabled by default.
//!
//! ## Main Types
//!
//! | Area          | Types                                                        |
//! |---------------|--------------------------------------------------------------|
//! | Handler       | [`ApiHandler`], [`ApiError`]                                 |
//! | Access        | [`ApiAuthenticator`], [`ApiAuthorizer`], [`ApiIdentity`]     |
//! | Streams       | [`TaskWatchEventStream`], [`OutputEventStream`]              |
//! | Metrics       | [`ApiMetricsBackend`], [`ApiMetricsHandle`], [`Transport`]   |
//! | HTTP          | `HttpApi`, `HttpApiParts`                                    |
//! | gRPC          | `GrpcApi`, `grpc::wire`                                      |
//! | Core adapter  | `SupervisorApiAdapter`                                       |
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
//! let http = HttpApi::new(handler).build();
//! # let _ = (grpc, http.router, http.openapi);
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

macro_rules! task_api_major {
    () => {
        1
    };
}

const _: () = assert!(
    solti_model::TASK_API_VERSION_MAJOR == task_api_major!(),
    "Task model and API transport major versions must match",
);

/// Compose a compile-time Kubernetes named-group URL rooted at `/apis/solti.io/v<API_MAJOR>`.
macro_rules! api_url {
    ($path:literal) => {
        concat!("/apis/solti.io/v", task_api_major!(), $path)
    };
}

/// Current public API major version.
pub const API_VERSION: u32 = solti_model::TASK_API_VERSION_MAJOR;

/// Current public API version name.
pub const API_VERSION_NAME: &str = concat!("v", task_api_major!());

/// Current gRPC package exposed by the agent.
pub const GRPC_API_PACKAGE: &str = concat!("solti.task.v", task_api_major!());

/// Current gRPC service exposed by the agent.
pub const GRPC_API_SERVICE: &str = concat!("solti.task.v", task_api_major!(), ".TaskService");

/// Root path of the HTTP Kubernetes API group.
pub const HTTP_API_ROOT: &str = api_url!("");

/// Maximum HTTP request body and gRPC message size.
///
/// The limit is 4 MiB.
pub const MAX_REQUEST_BYTES: usize = solti_model::MAX_TASK_MANIFEST_BYTES;

/// Maximum encoded body of one Task list response.
///
/// HTTP measures compact JSON without headers. gRPC measures the protobuf
/// message without its frame header.
pub const MAX_TASK_LIST_RESPONSE_BYTES: usize = solti_model::MAX_TASK_PAGE_ITEM_BYTES;

/// Maximum encoded body of one TaskRun list response.
///
/// HTTP measures compact JSON without headers. gRPC measures the protobuf
/// message without its frame header.
pub const MAX_TASK_RUN_LIST_RESPONSE_BYTES: usize = solti_model::MAX_TASK_RUN_PAGE_ITEM_BYTES;

mod error;
pub use error::{ApiConflict, ApiConflictReason, ApiError, ApiErrorCause};

mod auth;
pub use auth::{
    ApiAuthenticator, ApiAuthenticatorHandle, ApiAuthorizer, ApiAuthorizerHandle, ApiIdentity,
    AuthenticationRequest, AuthorizationRequest, TaskOperation, TaskTarget,
};

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
    include!("generated/solti.task.v1.rs");
}

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
pub use http::{HttpApi, HttpApiParts};

#[cfg(feature = "http")]
pub use axum;

#[cfg(feature = "http")]
pub use aide;

#[cfg(feature = "grpc-tls")]
mod tls;

#[cfg(feature = "grpc-tls")]
pub use tls::to_tonic_server_tls;

#[cfg(test)]
mod contract_identity_guard {
    #[test]
    fn task_contract_identity_is_consistent() {
        assert_eq!(super::API_VERSION_NAME, format!("v{}", super::API_VERSION));
        assert_eq!(
            super::GRPC_API_PACKAGE,
            format!("solti.task.v{}", super::API_VERSION),
        );
        assert_eq!(
            super::GRPC_API_SERVICE,
            format!("{}.TaskService", super::GRPC_API_PACKAGE),
        );
        assert_eq!(
            super::HTTP_API_ROOT.strip_prefix("/apis/"),
            Some(solti_model::TASK_API_VERSION),
            "HTTP named group must match the Task resource apiVersion",
        );
    }
}
