//! # solti-api - task management API.
//!
//! Dual-transport API layer exposing task operations over gRPC and HTTP. Both transports share the same wire types generated from `proto/solti/v1/*.proto` and delegate to an [`ApiHandler`] implementation.
//!
//! | feature | transport         | module                              |
//! |---------|-------------------|-------------------------------------|
//! | `grpc`  | tonic gRPC server | `SoltiApiService`, `SoltiApiServer` |
//! | `http`  | axum HTTP/JSON    | `HttpApi`                           |
//!
//! ## Quick start
//!
//! ```text
//! let adapter = SupervisorApiAdapter::new(supervisor);
//! let svc     = SoltiApiServer::new(SoltiApiService::new(Arc::new(adapter)));
//! let router  = HttpApi::new(Arc::new(adapter)).router();
//! ```
//!
//! ## Also
//!
//! - [`ApiHandler`] transport-agnostic trait with 6 operations.
//! - [`SupervisorApiAdapter`] default adapter bridging to `SupervisorApi`.
//! - [`ApiError`] unified error type mapped to gRPC Status / HTTP JSON.

/// Current API protocol version.
///
/// Bumped when the proto contract changes in a backwards-incompatible way.
/// Sent to control-plane via `solti_discover::DiscoverConfig`.
pub const API_VERSION: u32 = 1;

mod error;
pub use error::ApiError;

mod handler;
pub use handler::ApiHandler;

mod adapter;
pub use adapter::SupervisorApiAdapter;

#[cfg(any(feature = "grpc", feature = "http"))]
#[allow(dead_code)] // Generated proto types: request/response messages are exposed for
// wire completeness but only directly used by one transport at a time (gRPC uses all,
// HTTP reaches some via path params instead of request-typed bodies).
pub(crate) mod proto_api {
    include!(concat!(env!("OUT_DIR"), "/solti.v1.rs"));

    #[cfg(feature = "http")]
    include!(concat!(env!("OUT_DIR"), "/solti.v1.serde.rs"));
}

#[cfg(any(feature = "grpc", feature = "http"))]
mod convert;

#[cfg(any(feature = "grpc", feature = "http"))]
mod validate;

#[cfg(feature = "grpc")]
mod grpc;

#[cfg(feature = "grpc")]
pub use grpc::SoltiApiService;

#[cfg(feature = "grpc")]
pub use proto_api::solti_api_server::SoltiApiServer;

#[cfg(feature = "grpc")]
pub use tonic;

#[cfg(feature = "http")]
mod http;

#[cfg(feature = "http")]
pub use http::HttpApi;

#[cfg(feature = "http")]
pub use axum;
