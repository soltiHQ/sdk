//! # Task environment
//!
//! [`TaskEnv`] stores ordered key-value pairs.
//! Lookup uses the last matching value.

#[macro_use]
mod macros;

mod task;
pub use task::TaskEnv;
