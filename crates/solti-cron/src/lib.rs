//! Calendar scheduling for Taskvisor tasks.
//!
//! [`CronSchedule`] parses five-field cron expressions, or six fields with seconds,
//! in an explicit IANA timezone. [`CronTask`] runs an existing [`taskvisor::TaskRef`]
//! sequentially at future occurrences, inside one supervised attempt.
//! Taskvisor retains ownership of admission, retries, cancellation, and shutdown.
//! This crate does not spawn background workers or persist schedules.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// UTC calendar instants accepted and returned by the schedule API.
pub use chrono::{DateTime, Utc};
/// IANA timezone identifiers used by [`CronSchedule`].
pub use chrono_tz::Tz;

mod error;
pub use error::CronError;
mod schedule;
pub use schedule::CronSchedule;
mod task;
pub use task::CronTask;

/// Compiles runnable README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
