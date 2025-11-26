mod error;
pub use error::ExecError;

#[cfg(feature = "subprocess")]
pub mod subprocess;
