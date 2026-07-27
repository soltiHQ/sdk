//! Label selection types.
//!
//! [`LabelSelector`] is shared by runner routing and task queries.
//! `match_labels` and `match_expressions` are ANDed.
//!
//! - [`SelectorRequirement`] - single set-based requirement (`In`, `NotIn`, `Exists`, `DoesNotExist`).
//! - [`LabelSelector`] - top-level selector with `match_labels` and `match_expressions`.
//! - [`SelectorOperator`] - comparison operator enum.

mod requirement;
pub use requirement::SelectorRequirement;

mod operator;
pub use operator::SelectorOperator;

mod label;
pub use label::LabelSelector;
