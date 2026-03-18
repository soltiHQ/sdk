//! Runner selection types.
//!
//! ```text
//!  TaskSpec
//!  ┌──────────────────────────────────────────────────────┐
//!  │  runner_selector:                                    │
//!  │    matchLabels:      { "zone": "eu" }                │
//!  │    matchExpressions: [ {key:"gpu", op:Exists} ]      │
//!  └────────────────────────┬─────────────────────────────┘
//!                           │  ALL requirements ANDed
//!                           ▼
//!  RunnerRouter::pick()
//!  ┌──────────────────────────────────────────────────────┐
//!  │  Runner A  labels: {"zone":"us","gpu":"a100"}  ✗     │
//!  │  Runner B  labels: {"zone":"eu","gpu":"h100"}  ✓     │
//!  └──────────────────────────────────────────────────────┘
//! ```
//!
//! - [`RunnerSelector`]        — top-level selector with `match_labels` + `match_expressions`.
//! - [`SelectorOperator`]      — comparison operator enum.
//! - [`SelectorRequirement`]   — single set-based requirement (`In`, `NotIn`, `Exists`, `DoesNotExist`).

mod operator;
pub use operator::SelectorOperator;

mod requirement;
pub use requirement::SelectorRequirement;

mod runner;
pub use runner::RunnerSelector;
