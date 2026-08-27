//! # Supervisor builder
//!
//! [`SupervisorApiBuilder`] assembles the core runtime.
//!
//! ```text
//! RunnerRouter
//!      ├── Taskvisor runtime settings
//!      ├── controller settings
//!      ├── external subscribers
//!      ├── state admission and retention
//!      └── output capacity
//!              ▼
//!         SupervisorApi
//! ```

use std::sync::Arc;

use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

use super::SupervisorApi;
use crate::persistence::PersistenceSinks;
use crate::{
    CoreError, OutputConfig, PersistenceConfig, ReconciliationConfig, StateConfig,
    TaskOutputSinkHandle, TaskStateSinkHandle,
};

/// Builder for [`SupervisorApi`].
///
/// All settings have defaults.
/// A [`RunnerRouter`] is always required.
///
/// [`start`](Self::start) starts Taskvisor.
/// It also installs the state observer and retention worker.
#[must_use]
pub struct SupervisorApiBuilder {
    runtime_config: SupervisorConfig,
    controller_config: ControllerConfig,
    subscribers: Vec<Arc<dyn Subscribe>>,
    router: RunnerRouter,
    state_config: StateConfig,
    reconciliation_config: ReconciliationConfig,
    output_config: OutputConfig,
    persistence_config: PersistenceConfig,
    state_sink: Option<TaskStateSinkHandle>,
    output_sink: Option<TaskOutputSinkHandle>,
}

pub(super) struct SupervisorStartConfig {
    pub(super) runtime: SupervisorConfig,
    pub(super) controller: ControllerConfig,
    pub(super) subscribers: Vec<Arc<dyn Subscribe>>,
    pub(super) router: RunnerRouter,
    pub(super) state: StateConfig,
    pub(super) reconciliation: ReconciliationConfig,
    pub(super) output: OutputConfig,
    pub(super) persistence: PersistenceSinks,
}

impl SupervisorApiBuilder {
    /// Creates a builder with default settings.
    pub fn new(router: RunnerRouter) -> Self {
        Self {
            runtime_config: SupervisorConfig::default(),
            controller_config: ControllerConfig::default(),
            subscribers: Vec::new(),
            router,
            state_config: StateConfig::default(),
            reconciliation_config: ReconciliationConfig::default(),
            output_config: OutputConfig::default(),
            persistence_config: PersistenceConfig::default(),
            state_sink: None,
            output_sink: None,
        }
    }

    /// Replaces Taskvisor runtime settings.
    ///
    /// Taskvisor 0.9 shares `ownership_capacity` between configured subscribers
    /// and task values retained through intake, queuing, physical execution,
    /// and isolated destruction. Core always installs one subscriber for its
    /// state observer; each external subscriber consumes another slot.
    pub fn with_runtime_config(mut self, runtime_config: SupervisorConfig) -> Self {
        self.runtime_config = runtime_config;
        self
    }

    /// Replaces Taskvisor controller settings.
    pub fn with_controller_config(mut self, controller_config: ControllerConfig) -> Self {
        self.controller_config = controller_config;
        self
    }

    /// Replaces external Taskvisor subscribers.
    ///
    /// The core observer is installed separately.
    /// It cannot be replaced through this method.
    /// Taskvisor charges both the core observer and these subscribers against
    /// `SupervisorConfig::ownership_capacity`, together with retained task
    /// values.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Replaces state admission and retention settings.
    pub fn with_state_config(mut self, state_config: StateConfig) -> Self {
        self.state_config = state_config;
        self
    }

    /// Replaces runner-build admission and deadline settings.
    pub fn with_reconciliation_config(
        mut self,
        reconciliation_config: ReconciliationConfig,
    ) -> Self {
        self.reconciliation_config = reconciliation_config;
        self
    }

    /// Replaces live output settings.
    pub fn with_output_config(mut self, output_config: OutputConfig) -> Self {
        self.output_config = output_config;
        self
    }

    /// Replaces bounded persistence delivery settings.
    pub fn with_persistence_config(mut self, persistence_config: PersistenceConfig) -> Self {
        self.persistence_config = persistence_config;
        self
    }

    /// Installs a lossless task state persistence hook.
    ///
    /// Core delivers events on its dedicated persistence worker in commit order.
    /// Queue saturation applies bounded backpressure before the global state
    /// lock is acquired. The sink must eventually return so shutdown can drain it.
    /// Polling [`SupervisorApi::shutdown`] on the callback worker panics before
    /// shutdown starts. Waiting for another thread that calls shutdown can deadlock.
    /// [`SupervisorApi::state_persistence_status`] exposes delivery health and
    /// counters after startup.
    pub fn with_state_sink(mut self, sink: TaskStateSinkHandle) -> Self {
        self.state_sink = Some(sink);
        self
    }

    /// Installs a bounded, best-effort task output persistence hook.
    ///
    /// Core invokes the sink on a dedicated worker. Runner publication never
    /// waits for queue capacity. A full or unhealthy dispatcher drops only the
    /// external callback copy. [`SupervisorApi::output_persistence_status`]
    /// exposes admission, health, and delivery counters after startup.
    /// The sink must eventually return so shutdown can drain accepted events.
    /// Polling [`SupervisorApi::shutdown`] on the callback worker panics before
    /// shutdown starts. Waiting for another thread that calls shutdown can
    /// deadlock and is also forbidden.
    pub fn with_output_sink(mut self, sink: TaskOutputSinkHandle) -> Self {
        self.output_sink = Some(sink);
        self
    }

    /// Starts the supervisor and core-owned workers.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StateInitialization`] when state identity creation fails.
    /// Returns [`CoreError::PersistenceInitialization`] when a configured
    /// persistence worker cannot start.
    /// Returns [`CoreError::SupervisorInitialization`] when Taskvisor rejects
    /// the runtime, controller, or subscriber configuration.
    /// Returns [`CoreError::Supervisor`] when Taskvisor runtime startup fails.
    pub async fn start(self) -> Result<SupervisorApi, CoreError> {
        SupervisorApi::start(SupervisorStartConfig {
            runtime: self.runtime_config,
            controller: self.controller_config,
            subscribers: self.subscribers,
            router: self.router,
            state: self.state_config,
            reconciliation: self.reconciliation_config,
            output: self.output_config,
            persistence: PersistenceSinks {
                state: self.state_sink,
                output: self.output_sink,
                config: self.persistence_config,
            },
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::atomic::Ordering;
    use std::sync::{OnceLock, Weak};
    use std::task::{Context, Poll, Waker};

    use solti_model::{EmbeddedSpec, TaskId, TaskManifest, TaskSpec, TaskWorkload, Uid};

    use super::*;

    struct IgnoringStateSink;

    impl crate::TaskStateSink for IgnoringStateSink {
        fn on_event(&self, _event: &crate::TaskStateEvent) {}
    }

    struct IgnoringOutputSink;

    struct IgnoringTaskvisorSubscriber;

    impl Subscribe for IgnoringTaskvisorSubscriber {
        fn on_event(&self, _event: &taskvisor::Event) {}
    }

    impl crate::TaskOutputSink for IgnoringOutputSink {
        fn on_event(&self, _event: &crate::TaskOutputEvent) {}
    }

    struct ReentrantShutdownOutputSink {
        api: OnceLock<Weak<SupervisorApi>>,
        runtime: tokio::runtime::Handle,
    }

    struct ReentrantShutdownStateSink {
        api: OnceLock<Weak<SupervisorApi>>,
        runtime: tokio::runtime::Handle,
    }

    impl crate::TaskStateSink for ReentrantShutdownStateSink {
        fn on_event(&self, _event: &crate::TaskStateEvent) {
            let api = self
                .api
                .get()
                .and_then(Weak::upgrade)
                .expect("the test API remains alive");
            let _ = self.runtime.block_on(api.shutdown());
        }
    }

    fn persistence_test_manifest(name: &str) -> TaskManifest {
        TaskManifest::new(
            name,
            TaskSpec::builder(
                "persistence-test-slot",
                TaskWorkload::Embedded(EmbeddedSpec::new("persistence-test-v1").unwrap()),
                1_000_u64,
            )
            .build()
            .unwrap(),
        )
        .unwrap()
    }

    impl crate::TaskOutputSink for ReentrantShutdownOutputSink {
        fn on_event(&self, _event: &crate::TaskOutputEvent) {
            let api = self
                .api
                .get()
                .and_then(Weak::upgrade)
                .expect("the test API remains alive");
            let _ = self.runtime.block_on(api.shutdown());
        }
    }

    #[tokio::test]
    async fn defaults_start_and_shutdown() {
        let api = SupervisorApiBuilder::new(RunnerRouter::new())
            .start()
            .await
            .unwrap();
        assert!(api.state_persistence_status().is_none());
        assert!(api.output_persistence_status().is_none());
        api.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn taskvisor_build_rejection_is_returned_as_typed_start_error() {
        let runtime =
            SupervisorConfig::default().with_bus_capacity(NonZeroUsize::new(usize::MAX).unwrap());

        let error = match SupervisorApiBuilder::new(RunnerRouter::new())
            .with_runtime_config(runtime)
            .start()
            .await
        {
            Ok(_) => panic!("Taskvisor must reject a structurally invalid bus capacity"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            CoreError::SupervisorInitialization(taskvisor::BuildError::CapacityTooLarge {
                field: "bus_capacity",
                value: usize::MAX,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn taskvisor_ownership_capacity_includes_the_core_observer_and_external_subscribers() {
        let runtime = SupervisorConfig::default().with_ownership_capacity(NonZeroUsize::new(1));
        let error = match SupervisorApiBuilder::new(RunnerRouter::new())
            .with_runtime_config(runtime)
            .with_subscribers(vec![Arc::new(IgnoringTaskvisorSubscriber)])
            .start()
            .await
        {
            Ok(_) => panic!("two subscribers must not fit one ownership slot"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            CoreError::SupervisorInitialization(taskvisor::BuildError::ResourceLimitReached {
                resource: "owned_user_lifetimes",
                limit: 1,
                ..
            })
        ));
    }

    #[test]
    fn taskvisor_runtime_start_failure_keeps_the_start_operation_label() {
        let mut start = Box::pin(SupervisorApiBuilder::new(RunnerRouter::new()).start());
        let mut context = Context::from_waker(Waker::noop());
        let error = match start.as_mut().poll(&mut context) {
            Poll::Ready(Err(error)) => error,
            Poll::Ready(Ok(_)) => panic!("Taskvisor startup must require an active Tokio runtime"),
            Poll::Pending => panic!("startup without Tokio must fail synchronously"),
        };

        assert!(matches!(
            error,
            CoreError::Supervisor {
                op: "start",
                source: taskvisor::Error::Runtime(taskvisor::RuntimeError::TokioRuntimeUnavailable),
            }
        ));
    }

    #[tokio::test]
    async fn state_persistence_status_is_public_when_a_sink_is_installed() {
        let api = SupervisorApiBuilder::new(RunnerRouter::new())
            .with_state_sink(Arc::new(IgnoringStateSink))
            .start()
            .await
            .unwrap();
        let status = api
            .state_persistence_status()
            .expect("installed state sink must expose status");
        assert!(status.accepting());
        assert!(status.healthy());
        assert_eq!(status.queued(), 0);
        assert_eq!(status.capacity(), 2_049);
        assert_eq!(status.delivered(), 0);
        assert_eq!(status.failed(), 0);

        api.shutdown().await.unwrap();
        assert!(!api.state_persistence_status().unwrap().accepting());
    }

    #[tokio::test]
    async fn output_persistence_status_is_public_when_a_sink_is_installed() {
        let api = SupervisorApiBuilder::new(RunnerRouter::new())
            .with_output_sink(Arc::new(IgnoringOutputSink))
            .start()
            .await
            .unwrap();
        let status = api
            .output_persistence_status()
            .expect("installed output sink must expose status");
        assert!(status.accepting());
        assert!(status.healthy());
        assert_eq!(status.queued(), 0);
        assert_eq!(status.capacity(), 2_048);
        assert_eq!(status.delivered(), 0);
        assert_eq!(status.failed(), 0);
        assert_eq!(status.dropped(), 0);

        api.shutdown().await.unwrap();
        assert!(!api.output_persistence_status().unwrap().accepting());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_callback_cannot_reenter_supervisor_shutdown() {
        let sink = Arc::new(ReentrantShutdownOutputSink {
            api: OnceLock::new(),
            runtime: tokio::runtime::Handle::current(),
        });
        let api = Arc::new(
            SupervisorApiBuilder::new(RunnerRouter::new())
                .with_output_sink(sink.clone())
                .start()
                .await
                .unwrap(),
        );
        sink.api
            .set(Arc::downgrade(&api))
            .unwrap_or_else(|_| panic!("the test API is installed only once"));

        api.reconciler.output_hub.announce_run_started(
            &TaskId::new("reentrant-output-shutdown").unwrap(),
            &Uid::new("reentrant-output-shutdown-uid").unwrap(),
            1,
            1,
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while api.output_persistence_status().unwrap().healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reentrant callback panic must be isolated");

        assert!(
            !api.shutdown_started.load(Ordering::Acquire),
            "reentrant shutdown must panic before changing supervisor state"
        );
        let status = api.output_persistence_status().unwrap();
        assert!(!status.accepting());
        assert!(!status.healthy());
        assert_eq!(status.failed(), 1);

        tokio::time::timeout(std::time::Duration::from_secs(5), api.shutdown())
            .await
            .expect("external shutdown must not wait on the completed callback")
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            api.reconciler.output_hub.shutdown_persistence(),
        )
        .await
        .expect("output persistence shutdown remains idempotent");
        let status = api.output_persistence_status().unwrap();
        assert_eq!(status.queued(), 0);
        assert_eq!(status.delivered(), 0);
        assert_eq!(status.failed(), 1);
        assert_eq!(status.dropped(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_callback_cannot_reenter_supervisor_shutdown() {
        let sink = Arc::new(ReentrantShutdownStateSink {
            api: OnceLock::new(),
            runtime: tokio::runtime::Handle::current(),
        });
        let api = Arc::new(
            SupervisorApiBuilder::new(RunnerRouter::new())
                .with_state_sink(sink.clone())
                .start()
                .await
                .unwrap(),
        );
        sink.api
            .set(Arc::downgrade(&api))
            .unwrap_or_else(|_| panic!("the test API is installed only once"));

        api.reconciler
            .state
            .add_task(persistence_test_manifest("reentrant-state-shutdown"));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while api.state_persistence_status().unwrap().healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reentrant callback panic must be isolated");

        assert!(
            !api.shutdown_started.load(Ordering::Acquire),
            "reentrant shutdown must panic before changing supervisor state"
        );
        let status = api.state_persistence_status().unwrap();
        assert!(status.accepting());
        assert!(!status.healthy());
        assert_eq!(status.failed(), 1);

        tokio::time::timeout(std::time::Duration::from_secs(5), api.shutdown())
            .await
            .expect("external shutdown must not wait on the completed callback")
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            api.reconciler.state.shutdown_persistence(),
        )
        .await
        .expect("state persistence shutdown remains idempotent");
        let status = api.state_persistence_status().unwrap();
        assert!(!status.accepting());
        assert_eq!(status.queued(), 0);
        assert_eq!(status.delivered(), 0);
        assert_eq!(status.failed(), 1);
    }
}
