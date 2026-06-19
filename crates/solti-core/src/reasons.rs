//! # taskvisor event `reason` strings.
//!
//! taskvisor discriminates several terminal/rejection events **only** by the
//! free-form `reason` string on [`taskvisor::Event`] — there is no typed enum
//! for them (see `taskvisor::TaskOutcome` docs, which note `ActorExhausted` is
//! "discriminated only by its reason string"). solti-core depends on the exact
//! spelling to map an event to the correct [`TaskPhase`](solti_model::TaskPhase),
//! so a wrong match silently mis-classifies a run (e.g. a cancellation recorded
//! as a failure).
//!
//! These constants are the single source of truth for that coupling. They mirror
//! **taskvisor 0.3.0** internals and are **not** a stable taskvisor API.
//!
//! The `reason_contract` integration test drives a real taskvisor runtime and
//! asserts these strings are still emitted, so a taskvisor rename breaks CI here
//! instead of silently mis-mapping in production.

/// `ActorExhausted` reason for a task that completed successfully under a
/// `Never`/`OnFailure` policy (a normal one-shot completion, not an error).
pub const POLICY_EXHAUSTED_SUCCESS: &str = "policy_exhausted_success";

/// `ActorExhausted` reason for a task whose body returned `TaskError::Canceled`
/// without the runtime token being cancelled (a cooperative self-stop).
pub const TASK_RETURNED_CANCELED: &str = "task_returned_canceled";

/// `ControllerRejected` reason: a queued, not-yet-admitted submission was
/// removed from its slot queue (user-initiated cancel of a pending task).
pub const REMOVED_FROM_QUEUE: &str = "removed_from_queue";

/// `ControllerRejected` reason: a running task was superseded by a newer
/// submission under the `Replace` admission policy.
pub const SUPERSEDED_BY_REPLACE: &str = "superseded_by_replace";

/// `ControllerRejected` reason: a submission was dropped because the controller
/// is shutting down.
pub const CONTROLLER_SHUTTING_DOWN: &str = "controller_shutting_down";
