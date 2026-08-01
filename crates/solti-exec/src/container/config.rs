//! Container runner configuration.

use super::ContainerProcessPolicy;
use crate::output::LogConfig;

/// Settings shared by every task built by one container runner.
#[derive(Debug, Clone, Default)]
pub struct ContainerRunnerConfig {
    logger: LogConfig,
    process_policy: ContainerProcessPolicy,
}

impl ContainerRunnerConfig {
    /// Creates default container runner settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets stdout and stderr logging limits.
    pub fn with_logger(mut self, logger: LogConfig) -> Self {
        self.logger = logger;
        self
    }

    /// Sets low-level process controls for every container attempt.
    pub fn with_process_policy(mut self, policy: ContainerProcessPolicy) -> Self {
        self.process_policy = policy;
        self
    }

    pub(crate) fn prepare(self) -> Result<Self, crate::ExecError> {
        if self.logger.max_line_length == 0 {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "log_config.max_line_length cannot be zero".into(),
            ));
        }
        if self.logger.max_line_bytes == 0 {
            return Err(crate::ExecError::InvalidRunnerConfig(
                "log_config.max_line_bytes cannot be zero (all output would be swallowed)".into(),
            ));
        }
        self.process_policy
            .validate()
            .map_err(crate::ExecError::InvalidRunnerConfig)?;
        Ok(self)
    }

    pub(crate) fn logger(&self) -> LogConfig {
        self.logger
    }

    pub(crate) fn process_policy(&self) -> &ContainerProcessPolicy {
        &self.process_policy
    }
}
