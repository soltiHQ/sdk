//! # solti-api - task management API.
//!
//! Dual-transport API layer exposing task operations over gRPC and HTTP.
//! Both transports share the same wire types generated from `proto/solti/v1/*.proto` and delegate to an [`ApiHandler`] implementation.
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
//! - [`ApiError`] unified error type mapped to gRPC Status / HTTP JSON.
//! - [`SupervisorApiAdapter`] default adapter bridging to `SupervisorApi`.

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
pub use handler::ApiHandler;

mod adapter;
pub use adapter::SupervisorApiAdapter;

#[cfg(any(feature = "grpc", feature = "http"))]
#[cfg_attr(not(feature = "grpc"), allow(dead_code))]
pub(crate) mod proto_api {
    include!(concat!(
        env!("OUT_DIR"),
        "/solti.v",
        solti_api_major!(),
        ".rs"
    ));

    #[cfg(feature = "http")]
    include!(concat!(
        env!("OUT_DIR"),
        "/solti.v",
        solti_api_major!(),
        ".serde.rs"
    ));
}

#[cfg(any(feature = "grpc", feature = "http"))]
mod convert;

#[cfg(any(feature = "grpc", feature = "http"))]
mod validate;

#[cfg(feature = "grpc")]
mod grpc;

#[cfg(feature = "grpc")]
pub use grpc::{SoltiApiService, build_grpc_server};

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
