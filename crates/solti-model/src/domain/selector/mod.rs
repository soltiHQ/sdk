//! # Label selectors
//!
//! [`LabelSelector`] is shared by runner routing and task queries.
//! `match_labels` and `match_expressions` are ANDed.

mod requirement;
pub use requirement::SelectorRequirement;

mod operator;
pub use operator::SelectorOperator;

mod label;
pub use label::LabelSelector;
