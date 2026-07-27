//! Build and start a [`SupervisorApi`].

use std::sync::Arc;

use solti_runner::RunnerRouter;
use taskvisor::{ControllerConfig, Subscribe, SupervisorConfig};

use super::SupervisorApi;
use crate::{CoreError, OutputConfig, StateConfig};

/// Builder for a live [`SupervisorApi`].
///
/// [`start`](Self::start) creates and starts the Taskvisor runtime, installs the
/// state observer, and starts the internal retention worker.
#[must_use]
pub struct SupervisorApiBuilder {
    runtime_config: SupervisorConfig,
    controller_config: ControllerConfig,
    subscribers: Vec<Arc<dyn Subscribe>>,
    router: RunnerRouter,
    state_config: StateConfig,
    output_config: OutputConfig,
}

impl SupervisorApiBuilder {
    /// Create a builder with default runtime, controller, state, and output settings.
    pub fn new(router: RunnerRouter) -> Self {
        Self {
            runtime_config: SupervisorConfig::default(),
            controller_config: ControllerConfig::default(),
            subscribers: Vec::new(),
            router,
            state_config: StateConfig::default(),
            output_config: OutputConfig::default(),
        }
    }

    /// Replace Taskvisor runtime settings.
    pub fn with_runtime_config(mut self, runtime_config: SupervisorConfig) -> Self {
        self.runtime_config = runtime_config;
        self
    }

    /// Replace Taskvisor controller settings.
    pub fn with_controller_config(mut self, controller_config: ControllerConfig) -> Self {
        self.controller_config = controller_config;
        self
    }

    /// Replace external Taskvisor subscribers.
    ///
    /// The core state observer is installed separately and cannot be replaced.
    pub fn with_subscribers(mut self, subscribers: Vec<Arc<dyn Subscribe>>) -> Self {
        self.subscribers = subscribers;
        self
    }

    /// Replace state retention settings.
    pub fn with_state_config(mut self, state_config: StateConfig) -> Self {
        self.state_config = state_config;
        self
    }

    /// Replace live-output settings.
    pub fn with_output_config(mut self, output_config: OutputConfig) -> Self {
        self.output_config = output_config;
        self
    }

    /// Start the supervisor and every core-owned worker.
    pub async fn start(self) -> Result<SupervisorApi, CoreError> {
        SupervisorApi::start(
            self.runtime_config,
            self.controller_config,
            self.subscribers,
            self.router,
            self.state_config,
            self.output_config,
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
