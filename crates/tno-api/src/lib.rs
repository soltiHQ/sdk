#[cfg(feature = "grpc")]
mod proto {
    tonic::include_proto!("tno.v1");
}

mod error;
pub use error::ApiError;

mod handler;
pub use handler::ApiHandler;

mod adapter;
pub use adapter::SupervisorApiAdapter;

#[cfg(feature = "grpc")]
mod convert;

#[cfg(feature = "grpc")]
mod grpc;

#[cfg(feature = "grpc")]
pub use grpc::TnoApiService;

#[cfg(feature = "grpc")]
pub use proto::tno_api_server::TnoApiServer;

#[cfg(feature = "grpc")]
pub use tonic;

#[cfg(feature = "http")]
mod http;

#[cfg(feature = "http")]
pub use http::HttpApi;

#[cfg(feature = "http")]
pub use axum;
