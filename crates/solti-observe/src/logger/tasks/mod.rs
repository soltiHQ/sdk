//! # Optional periodic tasks.
//!
//! - [`timezone_sync`] (feature `timezone-sync`) re-detects local UTC offset.

#[cfg(feature = "timezone-sync")]
mod timezone_sync;
#[cfg(feature = "timezone-sync")]
pub use timezone_sync::timezone_sync;
