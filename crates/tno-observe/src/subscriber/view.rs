use std::borrow::Borrow;
use taskvisor::{Event, EventKind};
use tracing::{debug, error, info, trace, warn};

pub trait View {
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

#[inline]
pub fn message_for(kind: EventKind) -> &'static str {
    match kind {
        // management
        EventKind::TaskAdded => "task added (actor spawned and registered)",
        EventKind::TaskRemoved => "task removed (after join/cleanup)",
        EventKind::TaskRemoveRequested => "request to remove a task",
        EventKind::TaskAddRequested => "request to add a new task",

        // shutdown
        EventKind::GraceExceeded => "grace exceeded; some tasks did not stop in time",
        EventKind::AllStoppedWithinGrace => "all tasks stopped within grace period",
        EventKind::ShutdownRequested => "shutdown requested (OS signal)",

        // subscriber
        EventKind::SubscriberOverflow => {
            "event dropped for a subscriber (queue full or worker closed)"
        }
        EventKind::SubscriberPanicked => "subscriber panicked while processing an event",

        // terminal
        EventKind::ActorExhausted => "actor exhausted restart policy (no further restarts)",
        EventKind::ActorDead => "actor terminated permanently (fatal)",

        // lifecycle
        EventKind::TaskStopped => "task stopped (success or graceful cancel)",
        EventKind::TaskFailed => "task failed (non-fatal for this attempt)",
        EventKind::TimeoutHit => "task exceeded its configured timeout",
        EventKind::BackoffScheduled => "next attempt scheduled",
        EventKind::TaskStarting => "task is starting",

        // controller
        EventKind::ControllerRejected => "queue rejected",
        EventKind::ControllerSubmitted => "task submitted by controller",
        EventKind::ControllerSlotTransition => "controller slot transition",
    }
}

#[inline]
pub fn log_event<E: View>(e: E) {
    let msg = message_for(e.kind());

    match e.kind() {
        // management
        EventKind::TaskRemoveRequested => trace!(task = e.as_task(), "{msg}"),
        EventKind::TaskAddRequested => trace!(task = e.as_task(), "{msg}"),
        EventKind::TaskRemoved => trace!(task = e.as_task(), "{msg}"),
        EventKind::TaskAdded => debug!(task = e.as_task(), "{msg}"),

        // shutdown
        EventKind::ShutdownRequested => info!("{msg}"),
        EventKind::AllStoppedWithinGrace => info!("{msg}"),
        EventKind::GraceExceeded => warn!("{msg}"),

        // subscriber
        EventKind::SubscriberPanicked => {
            error!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::SubscriberOverflow => {
            error!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }

        // terminal
        EventKind::ActorExhausted => {
            debug!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }
        EventKind::ActorDead => {
            error!(task = e.as_task(), reason = e.as_reason(), "{msg}")
        }

        // lifecycle
        EventKind::TimeoutHit => {
            warn!(task = e.as_task(), timeout_ms = e.timeout_ms(), "{msg}")
        }
        EventKind::TaskStarting => {
            info!(task = e.as_task(), attempt = e.attempt(), "{msg}")
        }
        EventKind::TaskStopped => {
            trace!(task = e.as_task(), "{msg}")
        }
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
        EventKind::TaskFailed => error!(
            task = e.as_task(),
            attempt = e.attempt(),
            reason = e.as_reason(),
            "{msg}"
        ),

        // controller
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
