//! Core-owned live output hub and its public configuration/subscription ports.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::SystemTime;

use parking_lot::RwLock;
use solti_model::{OutputEvent, TaskId};
use solti_runner::{OutputPublisher, OutputSink};
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::ConfigError;

/// Configuration for per-task live output channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    capacity: NonZeroUsize,
}

impl OutputConfig {
    /// Default number of events retained by each task's lossy live-output ring.
    pub const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();

    /// Create output configuration with a per-task event capacity.
    pub const fn new(capacity: NonZeroUsize) -> Self {
        Self { capacity }
    }

    /// Create output configuration from a raw capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Zero`] when `capacity` is zero.
    pub const fn try_new(capacity: usize) -> Result<Self, ConfigError> {
        let Some(capacity) = NonZeroUsize::new(capacity) else {
            return Err(ConfigError::Zero {
                field: "output_capacity",
            });
        };
        Ok(Self::new(capacity))
    }

    /// Return the configured per-task event capacity.
    pub const fn capacity(self) -> NonZeroUsize {
        self.capacity
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

/// Lossy, live-only stream of task output events.
///
/// When a consumer falls behind the configured ring capacity, the stream emits
/// [`OutputEvent::Lagged`] and then continues with the newer retained events.
/// Terminal cleanup prevents new subscriptions. An existing subscription
/// closes after the hub evicts the channel and every outstanding runner sink
/// releases its sender.
pub struct OutputSubscription {
    inner: BroadcastStream<OutputEvent>,
}

impl OutputSubscription {
    fn new(receiver: broadcast::Receiver<OutputEvent>) -> Self {
        Self {
            inner: BroadcastStream::new(receiver),
        }
    }
}

impl Stream for OutputSubscription {
    type Item = OutputEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(skipped)))) => {
                Poll::Ready(Some(OutputEvent::Lagged { skipped }))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Concrete output hub owned exclusively by the supervisor layer.
pub(crate) struct OutputHub {
    channels: RwLock<HashMap<TaskId, broadcast::Sender<OutputEvent>>>,
    capacity: usize,
}

impl OutputHub {
    pub(crate) fn new(config: OutputConfig) -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            capacity: config.capacity().get(),
        }
    }

    /// Ensure a task channel exists and report whether this call created it.
    pub(crate) fn ensure_channel_if_absent(&self, task_id: TaskId) -> bool {
        let mut channels = self.channels.write();
        match channels.entry(task_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(broadcast::channel(self.capacity).0);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn ensure_channel(&self, task_id: TaskId) {
        self.ensure_channel_if_absent(task_id);
    }

    pub(crate) fn subscribe(&self, task_id: &TaskId) -> Option<OutputSubscription> {
        self.channels
            .read()
            .get(task_id)
            .map(|sender| OutputSubscription::new(sender.subscribe()))
    }

    #[cfg(test)]
    pub(crate) fn subscribe_raw(
        &self,
        task_id: &TaskId,
    ) -> Option<broadcast::Receiver<OutputEvent>> {
        self.channels
            .read()
            .get(task_id)
            .map(broadcast::Sender::subscribe)
    }

    pub(crate) fn announce_run_started(&self, task_id: &TaskId, generation: u64, attempt: u32) {
        self.send(
            task_id,
            OutputEvent::RunStarted {
                generation,
                attempt,
                started_at: SystemTime::now(),
            },
        );
    }

    pub(crate) fn announce_run_finished(
        &self,
        task_id: &TaskId,
        generation: u64,
        attempt: u32,
        exit_code: Option<i32>,
    ) {
        self.send(
            task_id,
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                finished_at: SystemTime::now(),
            },
        );
    }

    fn send(&self, task_id: &TaskId, event: OutputEvent) {
        if let Some(sender) = self.channels.read().get(task_id) {
            let _ = sender.send(event);
        }
    }

    pub(crate) fn evict(&self, task_id: &TaskId) {
        self.channels.write().remove(task_id);
    }

    #[cfg(test)]
    pub(crate) fn active_channels(&self) -> usize {
        self.channels.read().len()
    }
}

impl OutputPublisher for OutputHub {
    fn sink_for(&self, task_id: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        let sender = self.channels.read().get(task_id)?.clone();
        Some(OutputSink::new(generation, attempt, move |event| {
            let _ = sender.send(event);
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use solti_model::{OutputEvent, TaskId};
    use solti_runner::OutputPublisher;
    use tokio_stream::StreamExt;

    use super::{ConfigError, OutputConfig, OutputHub};

    #[test]
    fn config_preserves_default_and_rejects_zero() {
        assert_eq!(
            OutputConfig::default().capacity(),
            OutputConfig::DEFAULT_CAPACITY
        );
        assert_eq!(OutputConfig::DEFAULT_CAPACITY.get(), 256);
        assert_eq!(
            OutputConfig::try_new(0).unwrap_err(),
            ConfigError::Zero {
                field: "output_capacity"
            }
        );
        assert_eq!(OutputConfig::try_new(64).unwrap().capacity().get(), 64);
    }

    #[test]
    fn producer_cannot_create_a_task_channel() {
        let hub = OutputHub::new(OutputConfig::default());
        let task_id = TaskId::new("missing").unwrap();

        assert!(hub.sink_for(&task_id, 1, 1).is_none());
        assert_eq!(hub.active_channels(), 0);
    }

    #[tokio::test]
    async fn attempts_share_one_stream_with_run_markers() {
        let hub = OutputHub::new(OutputConfig::try_new(16).unwrap());
        let task_id = TaskId::new("retrying").unwrap();
        assert!(hub.ensure_channel_if_absent(task_id.clone()));
        let mut output = hub.subscribe(&task_id).expect("output subscription");

        hub.announce_run_started(&task_id, 1, 1);
        hub.sink_for(&task_id, 1, 1)
            .expect("attempt one sink")
            .stdout_line(Bytes::from_static(b"one"));
        hub.announce_run_finished(&task_id, 1, 1, Some(1));
        hub.announce_run_started(&task_id, 1, 2);
        hub.sink_for(&task_id, 1, 2)
            .expect("attempt two sink")
            .stderr_line(Bytes::from_static(b"two"));

        assert!(matches!(
            output.next().await,
            Some(OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                ..
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if chunk.generation == 1 && chunk.attempt == 1 && &chunk.line[..] == b"one"
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(1),
                ..
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::RunStarted {
                generation: 1,
                attempt: 2,
                ..
            })
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if chunk.generation == 1 && chunk.attempt == 2 && &chunk.line[..] == b"two"
        ));
    }

    #[tokio::test]
    async fn subscription_reports_lag_and_continues() {
        let hub = OutputHub::new(OutputConfig::try_new(1).unwrap());
        let task_id = TaskId::new("lagged").unwrap();
        hub.ensure_channel_if_absent(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("sink");

        sink.stdout_line(Bytes::from_static(b"one"));
        sink.stdout_line(Bytes::from_static(b"two"));

        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Lagged { skipped }) if skipped > 0
        ));
        assert!(matches!(
            output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"two"
        ));
    }

    #[tokio::test]
    async fn terminal_evict_waits_for_outstanding_sink_clones() {
        let hub = OutputHub::new(OutputConfig::try_new(8).unwrap());
        let task_id = TaskId::new("closing").unwrap();
        hub.ensure_channel_if_absent(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");
        let sink = hub.sink_for(&task_id, 1, 1).expect("sink");
        let outstanding = sink.clone();

        hub.evict(&task_id);
        assert!(hub.subscribe(&task_id).is_none());
        sink.stdout_line(Bytes::from_static(b"after-evict"));
        assert!(matches!(output.next().await, Some(OutputEvent::Chunk(_))));

        drop(sink);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), output.next())
                .await
                .is_err(),
            "the outstanding clone still owns the sender"
        );
        drop(outstanding);
        assert!(output.next().await.is_none());
    }

    #[tokio::test]
    async fn stale_sink_cannot_publish_into_a_reused_task_id() {
        let hub = Arc::new(OutputHub::new(OutputConfig::try_new(8).unwrap()));
        let task_id = TaskId::new("reused").unwrap();
        hub.ensure_channel_if_absent(task_id.clone());
        let mut old_output = hub.subscribe(&task_id).expect("old subscription");
        let stale_sink = hub.sink_for(&task_id, 1, 1).expect("old sink");

        hub.evict(&task_id);
        hub.ensure_channel_if_absent(task_id.clone());
        let mut new_output = hub.subscribe(&task_id).expect("new subscription");

        stale_sink.stdout_line(Bytes::from_static(b"stale"));
        assert!(matches!(
            old_output.next().await,
            Some(OutputEvent::Chunk(chunk)) if &chunk.line[..] == b"stale"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), new_output.next())
                .await
                .is_err(),
            "a stale sink must remain attached to the old generation"
        );
    }
}
