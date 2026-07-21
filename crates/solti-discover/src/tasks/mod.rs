//! # Embedded discovery tasks.
//!
//! Each factory returns a prebuilt runtime task and a complete desired Task resource.

mod sync;
mod transport;
pub use sync::sync;
