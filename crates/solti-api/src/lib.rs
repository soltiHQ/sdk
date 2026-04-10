//! # Task management API.
//!
//! Dual-transport API layer exposing task operations over gRPC and HTTP.
//! Both transports delegate to an [`ApiHandler`] trait implementation.
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
//! // gRPC
//! let svc = SoltiApiServer::new(SoltiApiService::new(Arc::new(adapter)));
//! // HTTP
//! let router = HttpApi::new(Arc::new(adapter)).router();
//! ```
//!
//! ## Also
//!
//! - [`ApiHandler`] transport-agnostic trait with 6 operations.
//! - [`SupervisorApiAdapter`] default adapter bridging to `SupervisorApi`.
//! - [`ApiError`] unified error type mapped to gRPC Status / HTTP JSON.

mod error;
pub use error::ApiError;

mod handler;
pub use handler::ApiHandler;

mod adapter;
pub use adapter::SupervisorApiAdapter;

#[cfg(feature = "grpc")]
mod proto_api {
    tonic::include_proto!("solti.v1");
}

#[cfg(feature = "grpc")]
mod convert;

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
