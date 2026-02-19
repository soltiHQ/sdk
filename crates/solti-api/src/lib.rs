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
