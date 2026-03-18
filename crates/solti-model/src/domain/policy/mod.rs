//! Lifecycle and concurrency policies for task execution.
//!
//! | Type               | Controls                                          | Default         |
//! |--------------------|---------------------------------------------------|-----------------|
//! | [`AdmissionPolicy`]| What happens when a slot is already occupied      | `DropIfRunning` |
//! | [`RestartPolicy`]  | Whether/when a task is restarted after completion | `Never`         |
//! | [`BackoffPolicy`]  | Exponential delay between restart attempts        | 1 s → 30 s × 2  |
//! | [`JitterPolicy`]   | Randomness applied to backoff delays              | `Full`          |

mod admission;
pub use admission::AdmissionPolicy;

mod backoff;
pub use backoff::BackoffPolicy;

mod jitter;
pub use jitter::JitterPolicy;

mod restart;
pub use restart::RestartPolicy;
