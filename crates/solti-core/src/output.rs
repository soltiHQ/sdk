//! # Live task output
//!
//! Core owns task output channels.
//! Runners receive only the publishing side.
//! Consumers receive only [`OutputSubscription`].
//!
//! ## Flow
//!
//! ```text
//! Runner
//!    │ OutputSink
//!    ▼
//! per-task broadcast ring
//!    │
//!    ▼
//! OutputSubscription
//! ```
//!
//! Output is live-only and best-effort.
//! It is not stored in task history.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::SystemTime;

use parking_lot::RwLock;
use solti_model::{OutputEvent, TaskId, Uid};
use solti_runner::{OutputPublisher, OutputSink};
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use crate::ConfigError;
use crate::persistence::{TaskOutputEvent, TaskOutputSinkHandle, publish_output_event};

/// Per-task live output settings.
///
/// The default capacity is [`Self::DEFAULT_CAPACITY`].
/// Capacity is measured in [`OutputEvent`] values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    capacity: NonZeroUsize,
}

impl OutputConfig {
    /// Default per-task event capacity.
    pub const DEFAULT_CAPACITY: NonZeroUsize = NonZeroUsize::new(256).unwrap();

    /// Creates settings with a non-zero event capacity.
    pub const fn new(capacity: NonZeroUsize) -> Self {
        Self { capacity }
    }

    /// Creates settings from a raw event capacity.
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

    /// Returns the per-task event capacity.
    pub const fn capacity(self) -> NonZeroUsize {
        self.capacity
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

/// Live stream of one task's output events.
///
/// The stream is lossy.
/// A slow consumer receives [`OutputEvent::Lagged`].
/// It then continues with newer events.
///
/// Terminal cleanup prevents new subscriptions.
/// An existing subscription closes after every runner sink releases its sender.
///
/// The stream implements [`tokio_stream::Stream`].
/// Its item type is [`OutputEvent`].
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

/// Core-owned output channel registry.
pub(crate) struct OutputHub {
    channels: RwLock<HashMap<TaskId, OutputChannel>>,
    capacity: usize,
    event_sink: Option<TaskOutputSinkHandle>,
}

struct OutputChannel {
    task_uid: Uid,
    sender: broadcast::Sender<OutputEvent>,
}

impl OutputHub {
    #[cfg(test)]
    pub(crate) fn new(config: OutputConfig) -> Self {
        Self::with_sink(config, None)
    }

    pub(crate) fn with_sink(
        config: OutputConfig,
        event_sink: Option<TaskOutputSinkHandle>,
    ) -> Self {
        Self {
            channels: RwLock::new(HashMap::new()),
            capacity: config.capacity().get(),
            event_sink,
        }
    }

    /// Ensures that a task channel exists.
    ///
    /// Returns `true` when this call creates it.
    pub(crate) fn ensure_channel_if_absent(&self, task_id: TaskId, task_uid: Uid) -> bool {
        let mut channels = self.channels.write();
        match channels.entry(task_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(OutputChannel {
                    task_uid,
                    sender: broadcast::channel(self.capacity).0,
                });
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn ensure_channel(&self, task_id: TaskId) -> Uid {
        let task_uid = Uid::new(format!("test-{task_id}")).expect("test UID");
        self.ensure_channel_if_absent(task_id, task_uid.clone());
        task_uid
    }

    pub(crate) fn subscribe(&self, task_id: &TaskId) -> Option<OutputSubscription> {
        self.channels
            .read()
            .get(task_id)
            .map(|channel| OutputSubscription::new(channel.sender.subscribe()))
    }

    #[cfg(test)]
    pub(crate) fn subscribe_raw(
        &self,
        task_id: &TaskId,
    ) -> Option<broadcast::Receiver<OutputEvent>> {
        self.channels
            .read()
            .get(task_id)
            .map(|channel| channel.sender.subscribe())
    }

    pub(crate) fn announce_run_started(
        &self,
        task_id: &TaskId,
        task_uid: &Uid,
        generation: u64,
        attempt: u32,
    ) {
        self.send(
            task_id,
            task_uid,
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
        task_uid: &Uid,
        generation: u64,
        attempt: u32,
        exit_code: Option<i32>,
    ) {
        self.send(
            task_id,
            task_uid,
            OutputEvent::RunFinished {
                generation,
                attempt,
                exit_code,
                finished_at: SystemTime::now(),
            },
        );
    }

    fn send(&self, task_id: &TaskId, task_uid: &Uid, event: OutputEvent) {
        if let Some(sender) = self
            .channels
            .read()
            .get(task_id)
            .map(|channel| channel.sender.clone())
        {
            let _ = sender.send(event.clone());
        }
        publish_output_event(
            self.event_sink.as_ref(),
            TaskOutputEvent::new(task_id.clone(), task_uid.clone(), event),
        );
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
        let channels = self.channels.read();
        let channel = channels.get(task_id)?;
        let sender = channel.sender.clone();
        let task_uid = channel.task_uid.clone();
        drop(channels);
        let event_sink = self.event_sink.clone();
        let task_id = task_id.clone();
        Some(OutputSink::new(generation, attempt, move |event| {
            let _ = sender.send(event.clone());
            publish_output_event(
                event_sink.as_ref(),
                TaskOutputEvent::new(task_id.clone(), task_uid.clone(), event),
            );
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use solti_model::{OutputEvent, TaskId, Uid};
    use solti_runner::OutputPublisher;
    use tokio_stream::StreamExt;

    use super::{ConfigError, OutputConfig, OutputHub};
    use crate::{TaskOutputEvent, TaskOutputSink, TaskOutputSinkHandle};

    #[derive(Default)]
    struct RecordingOutputSink {
        events: Mutex<Vec<TaskOutputEvent>>,
    }

    impl TaskOutputSink for RecordingOutputSink {
        fn on_event(&self, event: &TaskOutputEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

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
        let task_uid = hub.ensure_channel(task_id.clone());
        let mut output = hub.subscribe(&task_id).expect("output subscription");

        hub.announce_run_started(&task_id, &task_uid, 1, 1);
        hub.sink_for(&task_id, 1, 1)
            .expect("attempt one sink")
            .stdout_line(Bytes::from_static(b"one"));
        hub.announce_run_finished(&task_id, &task_uid, 1, 1, Some(1));
        hub.announce_run_started(&task_id, &task_uid, 1, 2);
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

    #[test]
    fn external_sink_receives_run_markers_and_first_chunk() {
        let recording = Arc::new(RecordingOutputSink::default());
        let sink: TaskOutputSinkHandle = recording.clone();
        let hub = OutputHub::with_sink(OutputConfig::try_new(16).unwrap(), Some(sink));
        let task_id = TaskId::new("persisted-output").unwrap();
        let task_uid = hub.ensure_channel(task_id.clone());

        hub.announce_run_started(&task_id, &task_uid, 1, 1);
        hub.sink_for(&task_id, 1, 1)
            .expect("attempt sink")
            .stdout_line(Bytes::from_static(b"first"));
        hub.announce_run_finished(&task_id, &task_uid, 1, 1, Some(0));

        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.task() == &task_id));
        assert!(events.iter().all(|event| event.task_uid() == &task_uid));
        assert!(matches!(
            events[0].event(),
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                ..
            }
        ));
        assert!(matches!(
            events[1].event(),
            OutputEvent::Chunk(chunk) if &chunk.line[..] == b"first"
        ));
        assert!(matches!(
            events[2].event(),
            OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(0),
                ..
            }
        ));
    }

    #[test]
    fn stale_output_keeps_the_original_task_uid() {
        let recording = Arc::new(RecordingOutputSink::default());
        let sink: TaskOutputSinkHandle = recording.clone();
        let hub = OutputHub::with_sink(OutputConfig::try_new(16).unwrap(), Some(sink));
        let task_id = TaskId::new("recreated-output").unwrap();
        let old_uid = Uid::new("old-output-incarnation").unwrap();
        let new_uid = Uid::new("new-output-incarnation").unwrap();

        assert!(hub.ensure_channel_if_absent(task_id.clone(), old_uid.clone()));
        let stale_sink = hub.sink_for(&task_id, 1, 1).expect("old sink");
        hub.evict(&task_id);
        assert!(hub.ensure_channel_if_absent(task_id.clone(), new_uid.clone()));
        let current_sink = hub.sink_for(&task_id, 1, 1).expect("new sink");

        stale_sink.stdout_line(Bytes::from_static(b"old"));
        current_sink.stdout_line(Bytes::from_static(b"new"));

        let events = recording.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].task_uid(), &old_uid);
        assert_eq!(events[1].task_uid(), &new_uid);
    }

    #[tokio::test]
    async fn subscription_reports_lag_and_continues() {
        let hub = OutputHub::new(OutputConfig::try_new(1).unwrap());
        let task_id = TaskId::new("lagged").unwrap();
        hub.ensure_channel(task_id.clone());
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
        hub.ensure_channel(task_id.clone());
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
        hub.ensure_channel(task_id.clone());
        let mut old_output = hub.subscribe(&task_id).expect("old subscription");
        let stale_sink = hub.sink_for(&task_id, 1, 1).expect("old sink");

        hub.evict(&task_id);
        hub.ensure_channel(task_id.clone());
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
