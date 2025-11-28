mod error;
pub use error::ExecError;

mod utils;

#[cfg(feature = "subprocess")]
pub mod subprocess;
