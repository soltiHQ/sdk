//! Conditional composite workload runner for Solti Tasks.
//!
//! A chain selects exactly one current step.
//! Each step can choose one next step after success and one after failure.
//! [`ChainSpec::validate`] requires every declared step to be reachable from `entry` and rejects cycles.
//! The model is an outcome-directed acyclic chain rather than a parallel DAG scheduler.
//!
//! The chain is carried through [`solti_model::TaskWorkload::Extension`] with [`CHAIN_API_VERSION`] and [`CHAIN_KIND`].
//! [`ChainRunner`] builds every nested workload through a snapshotted [`solti_runner::RunnerCatalog`] and returns one outer [`taskvisor::TaskRef`].
//!
//! Taskvisor therefore applies timeout, restart, backoff, admission, cancellation, status, and history to the whole chain attempt.
//!
//! Register leaf runners first, snapshot them into the chain runner, and then register the chain runner.
//! [`register_chain_runner`] performs the last two operations for the common case.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod error;
pub use error::{ChainError, ChainResult};

mod model;
pub use model::{
    CHAIN_API_VERSION, CHAIN_KIND, ChainSpec, ChainStep, FailureMode, FailureTransition,
    is_chain_workload,
};

mod output;

mod runner;
pub use runner::{ChainRunner, register_chain_runner};

#[cfg(feature = "schema")]
mod schema;
