use std::sync::Arc;

use tno_model::CreateSpec;
use tracing::{trace, instrument};

use crate::{
    error::CoreError,
    runner::{BuildContext, Runner},
};

#[derive(Default)]
pub struct RunnerRouter {
    runners: Vec<Arc<dyn Runner>>,
    ctx: BuildContext,
}

impl RunnerRouter {
    #[inline]
    pub fn new() -> Self {
        Self {
            runners: Vec::new(),
            ctx: BuildContext::default(),
        }
    }

    #[inline]
    pub fn with_context(mut self, ctx: BuildContext) -> Self {
        self.ctx = ctx;
        self
    }

    #[inline]
    pub fn register(&mut self, runner: Arc<dyn Runner>) {
        self.runners.push(runner);
    }

    pub fn pick(&self, spec: &CreateSpec) -> Option<&Arc<dyn Runner>> {
        self.runners.iter().find(|r| r.supports(spec))
    }

    #[instrument(level = "trace", skip(self, spec), fields(kind = ?spec.kind))]
    pub fn build(&self, spec: &CreateSpec) -> Result<taskvisor::TaskRef, CoreError> {
        let r = self
            .pick(spec)
            .ok_or_else(|| CoreError::NoRunner(format!("{:?}", spec.kind)))?;

        let task = r.build_task(spec, &self.ctx).map_err(CoreError::from)?;
        trace!(runner = r.name(), "runner built task successfully");
        Ok(task)
    }
}
