//! Optional supervised tasks.
//!
//! `timezone_sync` builds a task that tries to refresh the local UTC offset.

#[cfg(feature = "timezone-sync")]
mod timezone_sync;
#[cfg(feature = "timezone-sync")]
pub use timezone_sync::timezone_sync;
