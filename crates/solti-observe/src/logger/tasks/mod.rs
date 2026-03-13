// Periodic task that re-detects the local UTC offset.
// Enable with: `--features timezone-sync`
#[cfg(feature = "timezone-sync")]
mod timezone_sync;
#[cfg(feature = "timezone-sync")]
pub use timezone_sync::timezone_sync;
