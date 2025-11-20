//! High-level API over taskvisor `Supervisor` used by tno-core.
//! - Owns a `Supervisor` instance.
//! - Uses `RunnerRouter` to build tasks from `CreateSpec`.
//! - Submits tasks via the controller with mapped policies.
use std::sync::Arc;

use taskvisor::{Config as SupervisorConfig, ControllerConfig, Supervisor, Subscribe};
use tracing::{debug, info, instrument};

use crate::{
    error::CoreError,
    map::to_controller_spec,
    router::RunnerRouter,
};
use tno_model::CreateSpec;

/// Thin wrapper around taskvisor [`Supervisor`] with a runner router.
///
/// This type is responsible for:
/// - constructing and running the supervisor;
/// - selecting a concrete runner for each [`CreateSpec`];
/// - mapping model-level specs into controller specs and submitting them.
pub struct SupervisorApi {
    sup: Arc<Supervisor>,
    router: RunnerRouter,
}

impl SupervisorApi {
    /// Create a supervisor with explicit configs and start its run loop in background.
    ///
    /// `sup_cfg` — supervisor settings
    /// `ctrl_cfg` — controller settings
    /// `subscribers` — event subscribers
    /// `router` — runner router (Exec/Wasm/Container etc.)
    pub async fn new(
        sup_cfg: SupervisorConfig,
        ctrl_cfg: ControllerConfig,
        subscribers: Vec<Arc<dyn Subscribe>>,
        router: RunnerRouter,
    ) -> Result<Self, CoreError> {
        let sup = Supervisor::builder(sup_cfg)
            .with_subscribers(subscribers)
            .with_controller(ctrl_cfg)
            .build();

        let runner = Arc::clone(&sup);
        tokio::spawn(async move {
            if let Err(e) = runner.run(Vec::new()).await {
                panic!("supervisor run loop exited with error: {}", e)
            }
        });
        sup.wait_ready().await;
        info!("supervisor is ready to accept tasks");
        Ok(Self { sup, router })
    }

    /// Get a clone of the underlying supervisor handle.
    pub fn supervisor(&self) -> Arc<Supervisor> {
        Arc::clone(&self.sup)
    }

    /// Build and submit a task described by [`CreateSpec`].
    /// Steps:
    /// 1. Ask the router to pick a runner and build a `TaskRef`.
    /// 2. Map `CreateSpec` + task into a `ControllerSpec` using the adapter layer.
    /// 3. Submit the controller spec to the supervisor.
    #[instrument(level = "debug", skip(self, spec), fields(slot = %spec.slot, kind = ?spec.kind))]
    pub async fn submit(&self, spec: &CreateSpec) -> Result<(), CoreError> {
        let task = self.router.build(spec)?;

        debug!("submitting via controller");
        self.sup
            .submit(to_controller_spec(task, spec))
            .await
            .map_err(|e| CoreError::Supervisor(e.to_string()))
    }
}