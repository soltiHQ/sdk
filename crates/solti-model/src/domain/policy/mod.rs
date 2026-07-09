//! Lifecycle and concurrency policies for task execution.
//!
//! | Type                | Controls                                     | Default               |
//! |---------------------|----------------------------------------------|-----------------------|
//! | [`AdmissionPolicy`] | What happens when a slot is already occupied | `DropIfRunning`       |
//! | [`RestartPolicy`]   | Whether a task restarts after completion     | `Never`               |
//! | [`BackoffPolicy`]   | Delay between failure retries                | 1 s to 30 s, factor 2 |
//! | [`JitterPolicy`]    | Randomness applied to backoff delays         | `Full`                |

mod admission;
pub use admission::AdmissionPolicy;

mod backoff;
pub use backoff::BackoffPolicy;

mod jitter;
pub use jitter::JitterPolicy;

mod restart;
pub use restart::RestartPolicy;
