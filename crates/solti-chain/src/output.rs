//! Attempt-scoped output adaptation for nested workloads.

use std::future::Future;
use std::sync::Arc;

use solti_model::TaskId;
use solti_runner::{OutputPublisher, OutputPublisherHandle, OutputSink};

tokio::task_local! {
    /// Outer sink visible only while one chain-attempt future is being polled.
    static ATTEMPT_OUTPUT: Option<OutputSink>;
}

/// Creates the outer sink and the attempt-local adapter used by nested workloads.
pub(crate) struct ChainOutput {
    upstream: OutputPublisherHandle,
    task_name: TaskId,
    generation: u64,
}

/// Owns one outer attempt's sink.
pub(crate) struct AttemptOutput {
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
        })
    }

    pub(crate) fn publisher(&self) -> OutputPublisherHandle {
        Arc::new(ChildOutputPublisher)
    }

    pub(crate) fn begin(&self, attempt: u32) -> AttemptOutput {
        let sink = self
            .upstream
            .sink_for(&self.task_name, self.generation, attempt);
        AttemptOutput { sink }
    }
}

struct ChildOutputPublisher;

impl OutputPublisher for ChildOutputPublisher {
    fn sink_for(&self, _task_name: &TaskId, _generation: u64, _attempt: u32) -> Option<OutputSink> {
        ATTEMPT_OUTPUT.try_with(Clone::clone).ok().flatten()
    }
}

impl AttemptOutput {
    pub(crate) fn sink(&self) -> Option<&OutputSink> {
        self.sink.as_ref()
    }

    /// Polls one chain attempt with its sink installed for nested workloads.
    pub(crate) async fn scope<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        ATTEMPT_OUTPUT.scope(self.sink.clone(), future).await
    }
}
