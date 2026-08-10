//! # Supervisor builder
//!
//! [`SupervisorApiBuilder`] assembles the core runtime.
//!
//! ```text
//! RunnerRouter
//!      ├── Taskvisor runtime settings
//!      ├── controller settings
//!      ├── external subscribers
//!      ├── state retention
//!      └── output capacity
//!              ▼
//!         SupervisorApi
//! ```

use std::sync::Arc;

use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

use super::SupervisorApi;
use crate::persistence::PersistenceSinks;
use crate::{CoreError, OutputConfig, StateConfig, TaskOutputSinkHandle, TaskStateSinkHandle};

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
    output_config: OutputConfig,
    state_sink: Option<TaskStateSinkHandle>,
    output_sink: Option<TaskOutputSinkHandle>,
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
            output_config: OutputConfig::default(),
            state_sink: None,
            output_sink: None,
        }
    }

    /// Replaces Taskvisor runtime settings.
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
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Replaces state retention settings.
    pub fn with_state_config(mut self, state_config: StateConfig) -> Self {
        self.state_config = state_config;
        self
    }

    /// Replaces live output settings.
    pub fn with_output_config(mut self, output_config: OutputConfig) -> Self {
        self.output_config = output_config;
        self
    }

    /// Installs a synchronous task state persistence hook.
    ///
    /// Events are serialized in commit order and callbacks run outside the global state lock.
    /// The sink must return quickly and should forward events to an application-owned storage worker.
    pub fn with_state_sink(mut self, sink: TaskStateSinkHandle) -> Self {
        self.state_sink = Some(sink);
        self
    }

    /// Installs a synchronous task output persistence hook.
    ///
    /// The sink receives output from the first event.
    /// It must return quickly and should forward events to an application-owned storage worker.
    pub fn with_output_sink(mut self, sink: TaskOutputSinkHandle) -> Self {
        self.output_sink = Some(sink);
        self
    }

    /// Starts the supervisor and core-owned workers.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::StateInitialization`] when state identity creation fails.
    pub async fn start(self) -> Result<SupervisorApi, CoreError> {
        SupervisorApi::start(
            self.runtime_config,
            self.controller_config,
            self.subscribers,
            self.router,
            self.state_config,
            self.output_config,
            PersistenceSinks {
                state: self.state_sink,
                output: self.output_sink,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn defaults_start_and_shutdown() {
        let api = SupervisorApiBuilder::new(RunnerRouter::new())
            .start()
            .await
            .unwrap();
        api.shutdown().await.unwrap();
    }
}
