//! Task-provided environment variables.

// `#[macro_use]` must come before any module that uses the macro
// (Rust resolves `macro_rules!` top-to-bottom within a module).
#[macro_use]
mod macros;

mod task;
pub use task::TaskEnv;
