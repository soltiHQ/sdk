use thiserror::Error;

#[derive(Error, Debug)]
pub enum DiscoverError {
    #[error("failed to connect to control plane: {0}")]
    GrpcTransport(#[from] tonic::transport::Error),

    #[error("grpc call failed: {0}")]
    GrpcStatus(#[source] Box<tonic::Status>),

    #[error("http request failed: {0}")]
    HttpRequest(#[from] reqwest::Error),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("control plane rejected sync")]
    Rejected,
}

impl From<tonic::Status> for DiscoverError {
    fn from(status: tonic::Status) -> Self {
        DiscoverError::GrpcStatus(Box::new(status))
    }
}
