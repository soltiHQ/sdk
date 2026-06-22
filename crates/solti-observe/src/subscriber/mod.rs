//! Taskvisor event logger built on the [`tracing`] framework.
//!
//! [`TracingEventSubscriber`] implements [`Subscribe`] and maps every [`EventKind`] to the appropriate
//! tracing severity level with structured fields (`task`, `attempt`, `delay_ms`, `reason`, …).
//!
//! ## Log level mapping
//!
//! | Level   | Events                                                                               |
//! |---------|--------------------------------------------------------------------------------------|
//! | `trace` | TaskAddRequested, TaskRemoveRequested, TaskRemoved, TaskStopped, ControllerSubmitted |
//! | `debug` | TaskAdded, TaskCanceled, ActorExhausted, BackoffScheduled, ControllerSlotTransition   |
//! | `info`  | TaskStarting, ShutdownRequested, AllStoppedWithinGrace                               |
//! | `warn`  | GraceExceeded, TimeoutHit, ControllerRejected                                        |
//! | `error` | TaskFailed, ActorDead, SubscriberPanicked, SubscriberOverflow                        |
//!
//! ## Structured fields
//!
//! Each log line includes relevant structured fields from the event:
//!
//! | Field        | Type  | Present when                                 |
//! |--------------|-------|----------------------------------------------|
//! | `task`       | `str` | Most events (task name)                      |
//! | `attempt`    | `u32` | TaskStarting, TaskFailed, BackoffScheduled   |
//! | `reason`     | `str` | TaskFailed, ActorDead, SubscriberPanicked, … |
//! | `delay_ms`   | `u32` | BackoffScheduled                             |
//! | `timeout_ms` | `u32` | TimeoutHit                                   |

use std::borrow::Borrow;

use taskvisor::{BackoffSource, Event, EventKind, Subscribe};
use tracing::{debug, error, info, trace, warn};

/// Taskvisor event subscriber that logs every event via [`tracing`].
///
/// Zero-config: no fields, no constructor arguments.
/// Just wrap in `Arc` and register alongside other subscribers.
///
/// See the module-level docs for the complete event → log level mapping and the list of structured fields.
///
/// ## Example
///
/// ```text
/// use std::sync::Arc;
/// use solti_observe::TracingEventSubscriber;
/// use solti_prometheus::PrometheusSubscriber;
/// use taskvisor::Subscribe;
///
/// let subscribers: Vec<Arc<dyn Subscribe>> = vec![
///     Arc::new(TracingEventSubscriber),  // logs events via tracing
///     Arc::new(prom_subscriber),         // records events as metrics
/// ];
/// ```
///
/// ## Also
///
/// - [`log_event`] is a standalone function usable outside the subscriber pattern.
/// - [`View`] is a helper trait for extracting event fields with defaults.
/// - `solti-prometheus::PrometheusSubscriber` - complementary metrics subscriber.
#[derive(Default)]
pub struct TracingEventSubscriber;

/// Queue capacity sized for ~2K events/sec burst with sub-millisecond processing.
///
/// On overflow events are dropped and a [`EventKind::SubscriberOverflow`] event is emitted by taskvisor (non-blocking).
const QUEUE_CAPACITY: usize = 2048;

impl Subscribe for TracingEventSubscriber {
    /// Delegates to [`log_event`] which maps the event kind to the
    /// appropriate tracing macro at the correct severity level.
    fn on_event(&self, event: &Event) {
        log_event(event);
    }

    /// Returns `"tracing"` which used by the supervisor for diagnostics.
    fn name(&self) -> &'static str {
        "tracing"
    }

    /// Returns `2048`.
    fn queue_capacity(&self) -> usize {
        QUEUE_CAPACITY
    }
}

/// Logs a single event at the appropriate tracing level with structured fields.
///
/// Accepts anything that implements [`Borrow<Event>`], so both `&Event` and owned `Event` work transparently.
pub fn log_event<E: View>(e: E) {
    let msg = message_for(e.kind());

    match e.kind() {
        // Management - trace level for routine operations
        EventKind::TaskRemoveRequested => trace!(task = e.as_task(), "{msg}"),
        EventKind::TaskAddRequested => trace!(task = e.as_task(), "{msg}"),
        EventKind::TaskRemoved => trace!(task = e.as_task(), "{msg}"),
        EventKind::TaskAdded => debug!(task = e.as_task(), "{msg}"),

        // Shutdown - info/warn for lifecycle events
        EventKind::ShutdownRequested => info!("{msg}"),
        EventKind::AllStoppedWithinGrace => info!("{msg}"),
        EventKind::GraceExceeded => warn!("{msg}"),

        // Subscriber errors - always error level
        EventKind::SubscriberPanicked => {
            error!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::SubscriberOverflow => {
            error!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }

        // Terminal states - debug for exhausted, error for dead
        EventKind::ActorExhausted => {
            debug!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::ActorDead => {
            error!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }

        // Lifecycle
        EventKind::TimeoutHit => {
            warn!(task = e.as_task(), timeout_ms = e.timeout_ms(), "{msg}")
        }
        EventKind::TaskStarting => {
            info!(task = e.as_task(), attempt = e.attempt(), "{msg}")
        }
        EventKind::TaskCanceled => {
            debug!(task = e.as_task(), "{msg}")
        }
        EventKind::TaskStopped => {
            trace!(task = e.as_task(), "{msg}")
        }
        EventKind::TaskFailed => error!(
            task = e.as_task(),
            attempt = e.attempt(),
            reason = e.as_reason(),
            "{msg}"
        ),
        EventKind::BackoffScheduled => {
            let source = e.backoff_source();
            let msg = backoff_message(source);
            if matches!(source, Some(BackoffSource::Failure)) {
                debug!(
                    task = e.as_task(),
                    attempt = e.attempt(),
                    delay_ms = e.delay_ms(),
                    reason = e.as_reason(),
                    "{msg}",
                );
            } else {
                debug!(
                    task = e.as_task(),
                    attempt = e.attempt(),
                    delay_ms = e.delay_ms(),
                    "{msg}",
                );
            }
        }

        // Controller
        EventKind::ControllerRejected => {
            warn!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::ControllerSubmitted => {
            trace!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::ControllerSlotTransition => {
            debug!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }

        // Add failure (e.g. duplicate name) — no actor spawned.
        EventKind::TaskAddFailed => {
            warn!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }

        // `EventKind` is #[non_exhaustive]: tolerate variants added in future taskvisor releases.
        _ => debug!(task = e.as_task(), "{msg}"),
    }
}

/// Helper trait for extracting event fields with sensible defaults.
///
/// Blanket-implemented for anything that implements [`Borrow<Event>`].
/// Used internally by [`log_event`] to reduce boilerplate.
pub trait View {
    /// Task name, or `"unknown"` if absent.
    fn as_task(&self) -> &str;
    /// Reason string, or `"unknown"` if absent.
    fn as_reason(&self) -> &str;
    /// Attempt number, or `0` if absent.
    fn attempt(&self) -> u32;
    /// Backoff delay in milliseconds, or `0` if absent.
    fn delay_ms(&self) -> u32;
    /// Timeout in milliseconds, or `0` if absent.
    fn timeout_ms(&self) -> u32;
    /// The event kind.
    fn kind(&self) -> EventKind;
    /// Whether the event carries a reason field.
    fn has_reason(&self) -> bool;
    /// Canonical backoff source (success vs failure), if the event carries one.
    fn backoff_source(&self) -> Option<BackoffSource>;
}

impl<T> View for T
where
    T: Borrow<Event>,
{
    #[inline]
    fn as_task(&self) -> &str {
        self.borrow().task.as_deref().unwrap_or("unknown")
    }

    #[inline]
    fn as_reason(&self) -> &str {
        self.borrow().reason.as_deref().unwrap_or("unknown")
    }

    #[inline]
    fn attempt(&self) -> u32 {
        self.borrow().attempt.unwrap_or(0)
    }

    #[inline]
    fn delay_ms(&self) -> u32 {
        self.borrow().delay_ms.unwrap_or(0)
    }

    #[inline]
    fn timeout_ms(&self) -> u32 {
        self.borrow().timeout_ms.unwrap_or(0)
    }

    #[inline]
    fn kind(&self) -> EventKind {
        self.borrow().kind
    }

    #[inline]
    fn has_reason(&self) -> bool {
        self.borrow().reason.is_some()
    }

    #[inline]
    fn backoff_source(&self) -> Option<BackoffSource> {
        self.borrow().backoff_source
    }
}

/// Message for a `BackoffScheduled` event, chosen by the canonical [`BackoffSource`] instead of the fragile presence of a `reason` field.
#[inline]
fn backoff_message(source: Option<BackoffSource>) -> &'static str {
    match source {
        Some(BackoffSource::Failure) => "retry scheduled after failure",
        Some(BackoffSource::Success) => "next run scheduled after success",
        None => "next attempt scheduled",
    }
}

/// Human-readable description for each event kind.
///
/// Used as the primary log message; structured fields provide additional context.
#[inline]
fn message_for(kind: EventKind) -> &'static str {
    match kind {
        // Management
        EventKind::TaskAdded => "task added (actor spawned and registered)",
        EventKind::TaskRemoved => "task removed (after join/cleanup)",
        EventKind::TaskRemoveRequested => "request to remove a task",
        EventKind::TaskAddRequested => "request to add a new task",

        // Shutdown
        EventKind::GraceExceeded => "grace exceeded; some tasks did not stop in time",
        EventKind::AllStoppedWithinGrace => "all tasks stopped within grace period",
        EventKind::ShutdownRequested => "shutdown requested (OS signal)",

        // Subscriber
        EventKind::SubscriberOverflow => {
            "event dropped for a subscriber (queue full or worker closed)"
        }
        EventKind::SubscriberPanicked => "subscriber panicked while processing an event",

        // Terminal
        EventKind::ActorExhausted => "actor exhausted restart policy (no further restarts)",
        EventKind::ActorDead => "actor terminated permanently (fatal)",

        // Lifecycle
        EventKind::TaskCanceled => "task canceled (graceful cancellation)",
        EventKind::TaskStopped => "task stopped (success or graceful cancel)",
        EventKind::TaskFailed => "task failed (non-fatal for this attempt)",
        EventKind::TimeoutHit => "task exceeded its configured timeout",
        EventKind::BackoffScheduled => "next attempt scheduled",
        EventKind::TaskStarting => "task is starting",

        // Controller
        EventKind::ControllerRejected => "queue rejected",
        EventKind::ControllerSubmitted => "task submitted by controller",
        EventKind::ControllerSlotTransition => "controller slot transition",

        // Add failure (e.g. duplicate name)
        EventKind::TaskAddFailed => "task add failed (name already registered)",

        // `EventKind` is #[non_exhaustive]
        _ => "unknown event",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_canceled_has_a_dedicated_message_not_the_unknown_fallback() {
        let msg = message_for(EventKind::TaskCanceled);
        assert_ne!(
            msg, "unknown event",
            "graceful TaskCanceled must not fall through to the unknown-event fallback",
        );
        assert!(
            msg.contains("cancel"),
            "message should describe cancellation, got: {msg}",
        );
    }

    #[test]
    fn backoff_message_uses_canonical_source_not_reason_presence() {
        use taskvisor::BackoffSource;
        assert_eq!(
            backoff_message(Some(BackoffSource::Failure)),
            "retry scheduled after failure",
        );
        assert_eq!(
            backoff_message(Some(BackoffSource::Success)),
            "next run scheduled after success",
        );
        assert_eq!(
            backoff_message(None),
            "next attempt scheduled",
            "absent backoff source must not be misclassified as success or failure",
        );
    }
}
