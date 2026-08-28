//! Bounded, explicitly released callback fixtures shared by persistence and shutdown cases.

#![allow(dead_code)]

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use solti_benches::fixtures::{WAIT_BOUND, bounded};
use solti_core::{SupervisorApi, TaskOutputEvent, TaskOutputSink, TaskStateEvent, TaskStateSink};
use solti_model::OutputEvent;
use tokio::sync::Notify;

#[derive(Default)]
pub struct Progress {
    count: AtomicUsize,
    changed: Notify,
}

impl Progress {
    pub fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn advance(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }

    pub async fn wait(&self, minimum: usize) {
        bounded(async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.get() >= minimum {
                    break;
                }
                changed.await;
            }
        })
        .await;
    }
}

pub struct CallbackGate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl CallbackGate {
    pub fn new(open: bool) -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(open),
            changed: Condvar::new(),
        })
    }

    pub fn pause(&self) {
        *self.open.lock().unwrap() = false;
    }

    pub fn release(&self) {
        *self
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        self.changed.notify_all();
    }

    pub fn wait(&self) {
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT_BOUND, |open| !*open)
            .unwrap();
        let released = *open;
        drop(open);
        assert!(
            released,
            "benchmark callback was not released within its failure bound"
        );
    }
}

/// A panic in an assertion must not leave an SDK callback permanently blocked.
pub struct ReleaseOnDrop(pub Arc<CallbackGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

pub struct StateSink {
    pub gate: Arc<CallbackGate>,
    pub entered: Progress,
    pub delivered: Progress,
    work: usize,
}

impl StateSink {
    pub fn new(work: usize) -> Arc<Self> {
        Arc::new(Self {
            gate: CallbackGate::new(true),
            entered: Progress::default(),
            delivered: Progress::default(),
            work,
        })
    }
}

impl TaskStateSink for StateSink {
    fn on_event(&self, event: &TaskStateEvent) {
        self.entered.advance();
        self.gate.wait();
        std::hint::black_box(event);
        let mut value = 0_u64;
        for index in 0..self.work {
            value = std::hint::black_box(value.wrapping_add(index as u64));
        }
        std::hint::black_box(value);
        self.delivered.advance();
    }
}

pub struct OutputSink {
    pub gate: Arc<CallbackGate>,
    pub entered: Progress,
    pub chunks: AtomicUsize,
}

impl OutputSink {
    pub fn paused() -> Arc<Self> {
        Arc::new(Self {
            gate: CallbackGate::new(false),
            entered: Progress::default(),
            chunks: AtomicUsize::new(0),
        })
    }
}

impl TaskOutputSink for OutputSink {
    fn on_event(&self, event: &TaskOutputEvent) {
        self.entered.advance();
        self.gate.wait();
        if matches!(event.event(), OutputEvent::Chunk(_)) {
            self.chunks.fetch_add(1, Ordering::AcqRel);
        }
    }
}

pub async fn drain_state(api: &SupervisorApi) {
    bounded(async {
        loop {
            let status = api
                .state_persistence_status()
                .expect("configured state sink");
            assert!(status.healthy(), "state callback failed");
            if status.queued() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}
