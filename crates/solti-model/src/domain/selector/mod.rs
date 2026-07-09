//! Runner selection types.
//!
//! ```text
//! TaskSpec.runner_selector
//!   match_labels + match_expressions
//!   all requirements are ANDed
//!   matched against runner labels
//! ```
//!
//! - [`SelectorRequirement`] - single set-based requirement (`In`, `NotIn`, `Exists`, `DoesNotExist`).
//! - [`RunnerSelector`] - top-level selector with `match_labels` and `match_expressions`.
//! - [`SelectorOperator`] - comparison operator enum.

mod requirement;
pub use requirement::SelectorRequirement;

mod operator;
pub use operator::SelectorOperator;

mod runner;
pub use runner::RunnerSelector;
