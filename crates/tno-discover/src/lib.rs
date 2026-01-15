pub mod proto {
    tonic::include_proto!("tno.discover.v1");
}
pub use proto::*;

mod tasks;
pub use tasks::sync;

mod config;
pub use config::DiscoverConfig;
pub use config::DiscoveryTransport;

mod errors;
pub use errors::DiscoverError;
