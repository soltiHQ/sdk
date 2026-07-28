//! # Taskvisor integration
//!
//! This module owns runtime reconciliation and event projection.
//!
//! ```text
//! desired Task ──► Reconciler ──► Taskvisor
//!                                    ▼
//!                              RuntimeObserver
//!                                    ▼
//!                                 TaskState
//! ```
//!
//! Per-task locks serialize conflicting management operations.

mod locks;
mod observer;
mod reconciler;

pub(crate) use locks::TaskLocks;
pub(crate) use observer::RuntimeObserver;
pub(crate) use reconciler::{Reconciler, RuntimeSource};
