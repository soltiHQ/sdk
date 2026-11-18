//! # Embedded discovery task
//!
//! [`sync()`] returns a complete desired resource and its prebuilt runtime task.
//!
//! ```text
//! DiscoverConfig + UptimeSource
//!             ▼
//!           sync
//!             ├──► TaskManifest
//!             └──► taskvisor::TaskRef
//! ```

mod schemas;

mod sync;
pub use sync::sync;

mod transport;
