//! Attempt-scoped output adaptation for nested workloads.

use std::sync::Arc;

use parking_lot::Mutex;
use solti_model::TaskId;
use solti_runner::{OutputPublisher, OutputPublisherHandle, OutputSink};

/// Shares one outer sink between every nested workload in a chain attempt.
pub(crate) struct ChainOutput {
    upstream: OutputPublisherHandle,
    task_name: TaskId,
    generation: u64,
    active: Mutex<Option<ActiveOutput>>,
}

struct ActiveOutput {
    attempt: u32,
    sink: Option<OutputSink>,
}

impl ChainOutput {
    pub(crate) fn new(
        upstream: OutputPublisherHandle,
        task_name: TaskId,
        generation: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            upstream,
            task_name,
            generation,
            active: Mutex::new(None),
        })
    }

    pub(crate) fn publisher(self: &Arc<Self>) -> OutputPublisherHandle {
        Arc::new(ChildOutputPublisher {
            output: Arc::clone(self),
        })
    }

    pub(crate) fn begin(self: &Arc<Self>, attempt: u32) -> AttemptOutputGuard {
        let sink = self
            .upstream
            .sink_for(&self.task_name, self.generation, attempt);
        *self.active.lock() = Some(ActiveOutput {
            attempt,
            sink: sink.clone(),
        });
        AttemptOutputGuard {
            output: Arc::clone(self),
            attempt,
            sink,
        }
    }

    fn active_sink(&self) -> Option<OutputSink> {
        self.active
            .lock()
            .as_ref()
            .and_then(|active| active.sink.clone())
    }
}

struct ChildOutputPublisher {
    output: Arc<ChainOutput>,
}

impl OutputPublisher for ChildOutputPublisher {
    fn sink_for(&self, _task_name: &TaskId, _generation: u64, _attempt: u32) -> Option<OutputSink> {
        self.output.active_sink()
    }
}

/// Clears the installed sink when an attempt completes or its future is dropped.
pub(crate) struct AttemptOutputGuard {
    output: Arc<ChainOutput>,
    attempt: u32,
    sink: Option<OutputSink>,
}

impl AttemptOutputGuard {
    pub(crate) fn sink(&self) -> Option<&OutputSink> {
        self.sink.as_ref()
    }
}

impl Drop for AttemptOutputGuard {
    fn drop(&mut self) {
        let mut active = self.output.active.lock();
        if active
            .as_ref()
            .is_some_and(|active| active.attempt == self.attempt)
        {
            *active = None;
        }
    }
}
