#![cfg(feature = "subscriber")]

//! Event logging subscriber for Taskvisor.
//!
//! Maps Taskvisor events to structured tracing logs with appropriate severity levels.
//! Processes events asynchronously via bounded queue to avoid blocking the event system.

use std::borrow::Borrow;

use async_trait::async_trait;
use taskvisor::{Event, EventKind, Subscribe};
use tracing::{debug, error, info, trace, warn};

/// Subscriber that logs all Taskvisor events using the tracing framework.
///
/// Events are processed asynchronously with structured fields (task, attempt, etc.).
/// Queue overflow results in `SubscriberOverflow` events being emitted.
#[derive(Default)]
pub struct Subscriber;

/// Queue capacity sized for ~2K events/sec burst with sub-millisecond processing.
/// On overflow, events are dropped and `SubscriberOverflow` event is emitted (non-blocking).
const SUBSCRIBER_QUEUE_CAPACITY: usize = 2048;

#[async_trait]
impl Subscribe for Subscriber {
    async fn on_event(&self, event: &Event) {
        log_event(event);
    }

    fn name(&self) -> &'static str {
        "subscriber"
    }

    fn queue_capacity(&self) -> usize {
        SUBSCRIBER_QUEUE_CAPACITY
    }
}

/// Logs an event with appropriate tracing level and structured fields.
///
/// This is public to allow custom subscribers to reuse the same logging logic.
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

        // Lifecycle events
        EventKind::TimeoutHit => {
            warn!(task = e.as_task(), timeout_ms = e.timeout_ms(), "{msg}")
        }
        EventKind::TaskStarting => {
            info!(task = e.as_task(), attempt = e.attempt(), "{msg}")
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

        // Backoff - differentiate retry vs scheduled next run
        EventKind::BackoffScheduled => {
            if e.has_reason() {
                debug!(
                    task = e.as_task(),
                    attempt = e.attempt(),
                    delay_ms = e.delay_ms(),
                    reason = e.as_reason(),
                    "retry scheduled after failure",
                );
            } else {
                debug!(
                    task = e.as_task(),
                    attempt = e.attempt(),
                    delay_ms = e.delay_ms(),
                    "next run scheduled after success",
                );
            }
        }

        // Controller events
        EventKind::ControllerRejected => {
            warn!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::ControllerSubmitted => {
            trace!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::ControllerSlotTransition => {
            debug!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
    }
}

/// Helper trait for extracting event fields with sensible defaults.
///
/// This is internal to reduce boilerplate in `log_event`.
trait View {
    fn as_task(&self) -> &str;
    fn as_reason(&self) -> &str;
    fn attempt(&self) -> u32;
    fn delay_ms(&self) -> u32;
    fn timeout_ms(&self) -> u32;
    fn kind(&self) -> EventKind;
    fn has_reason(&self) -> bool;
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
}

/// Returns a human-readable description for each event kind.
///
/// These messages are used as the primary log message, with structured fields providing additional context.
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
        EventKind::TaskStopped => "task stopped (success or graceful cancel)",
        EventKind::TaskFailed => "task failed (non-fatal for this attempt)",
        EventKind::TimeoutHit => "task exceeded its configured timeout",
        EventKind::BackoffScheduled => "next attempt scheduled",
        EventKind::TaskStarting => "task is starting",

        // Controller
        EventKind::ControllerRejected => "queue rejected",
        EventKind::ControllerSubmitted => "task submitted by controller",
        EventKind::ControllerSlotTransition => "controller slot transition",
    }
}
