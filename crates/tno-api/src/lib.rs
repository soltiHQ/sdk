mod error;
pub use error::ApiError;

mod handler;
pub use handler::ApiHandler;

mod adapter;
pub use adapter::SupervisorApiAdapter;

#[cfg(feature = "grpc")]
mod proto_api {
    tonic::include_proto!("tno.v1");
}

#[cfg(feature = "grpc")]
mod convert;

#[cfg(feature = "grpc")]
mod grpc;

#[cfg(feature = "grpc")]
pub use grpc::TnoApiService;

#[cfg(feature = "grpc")]
pub use proto_api::tno_api_server::TnoApiServer;

#[cfg(feature = "grpc")]
pub use tonic;

#[cfg(feature = "http")]
mod http;

#[cfg(feature = "http")]
pub use http::HttpApi;

#[cfg(feature = "http")]
pub use axum;

#[cfg(feature = "discovery")]
mod proto_autodiscovery {
    tonic::include_proto!("lighthouse.v1");
}

#[cfg(feature = "discovery")]
mod discovery;

#[cfg(feature = "discovery")]
pub use discovery::DiscoveryConfig;

#[cfg(feature = "discovery")]
pub use discovery::build_heartbeat_task;
