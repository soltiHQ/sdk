//! # Supervised logger tasks
//!
//! `timezone_sync` builds the optional local-offset refresh task.

#[cfg(feature = "timezone-sync")]
mod timezone_sync;
#[cfg(feature = "timezone-sync")]
pub use timezone_sync::timezone_sync;
