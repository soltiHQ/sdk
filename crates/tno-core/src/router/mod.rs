//! Runner router that selects an appropriate `Runner` implementation for a given `CreateSpec`.
//!
//! The router checks registered runners in order and delegates task construction to the first one that reports `supports(spec) == true`.
use std::sync::Arc;

use tno_model::{CreateSpec, TaskKind};
use tracing::{debug, instrument, trace};

use crate::{
    error::CoreError,
    runner::{BuildContext, Runner},
};

/// Router that selects an appropriate [`Runner`] for a given [`CreateSpec`].
///
/// Runners are checked in the order they were registered. The first runner whose [`Runner::supports`] method returns `true` is used to build the task.
#[derive(Default)]
pub struct RunnerRouter {
    runners: Vec<Arc<dyn Runner>>,
    ctx: BuildContext,
}

impl RunnerRouter {
    /// Create an empty router with a default build context.
    #[inline]
    pub fn new() -> Self {
        Self {
            runners: Vec::new(),
            ctx: BuildContext::default(),
        }
    }

    /// Set a custom build context for all runners managed by this router.
    ///
    /// This is typically used to inject shared dependencies (config,
    /// observability, global handles, etc.) into runner instances.
    #[inline]
    pub fn with_context(mut self, ctx: BuildContext) -> Self {
        self.ctx = ctx;
        self
    }

    /// Register a new runner.
    ///
    /// Runners are queried in the order they are registered; the first
    /// one that reports `supports(spec) == true` will be used.
    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) {
        self.runners.push(runner);
    }

    /// Pick the first runner that claims to support the given spec.
    ///
    /// Returns `None` if no runner accepts this spec (e.g. unknown `TaskKind`).
    pub fn pick(&self, spec: &CreateSpec) -> Option<&Arc<dyn Runner>> {
        self.runners.iter().find(|r| r.supports(spec))
    }

    /// Build a [`taskvisor::TaskRef`] for the given spec using the selected runner.
    ///
    /// `TaskKind::None` is not routable and must be used with [`SupervisorApi::submit_with_task`](crate::supervisor::SupervisorApi::submit_with_task).
    #[instrument(level = "debug", skip(self, spec), fields(kind = ?spec.kind, slot = ?spec.slot))]
    pub fn build(&self, spec: &CreateSpec) -> Result<taskvisor::TaskRef, CoreError> {
        trace!(spec = ?spec, "router received spec");

        if matches!(spec.kind, TaskKind::None) {
            return Err(CoreError::NoRunner(
                "TaskKind::None requires submit_with_task()".to_string(),
            ));
        }
        let r = self
            .pick(spec)
            .ok_or_else(|| CoreError::NoRunner(spec.kind.kind().to_string()))?;

        let task = r.build_task(spec, &self.ctx).map_err(CoreError::from)?;
        debug!(runner = r.name(), "runner built task successfully");
        Ok(task)
    }
}
