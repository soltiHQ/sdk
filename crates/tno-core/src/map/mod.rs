//! Adapter layer between `tno-model` (public specs) and the taskvisor runtime.
//!
//! This crate maps high-level API types into taskvisor’s internal execution structures.

mod mapping;
pub use mapping::*;
