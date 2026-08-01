//! Container engine boundary.

use std::{error::Error, fmt, pin::Pin};

use async_trait::async_trait;
use tokio::io::AsyncRead;

/// Asynchronous byte stream returned by a container engine.
pub type ContainerOutput = Pin<Box<dyn AsyncRead + Send + 'static>>;

/// Classification used when an engine operation fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerErrorClass {
    /// The same task attempt may succeed later.
    Retryable,
    /// Retrying the unchanged task cannot fix the failure.
    Permanent,
}

/// Error returned by a container engine.
///
/// The engine classifies the error.
/// The runner maps retryable create, start, and wait errors to `TaskError::Fail`.
/// It maps their permanent errors to `TaskError::Fatal`.
/// A final termination or cleanup error is always fatal for the attempt.
pub struct ContainerEngineError {
    class: ContainerErrorClass,
    reason: String,
    source: Option<Box<dyn Error + Send + Sync + 'static>>,
}

impl ContainerEngineError {
    /// Creates a retryable engine error.
    pub fn retryable(reason: impl Into<String>) -> Self {
        Self {
            class: ContainerErrorClass::Retryable,
            reason: reason.into(),
            source: None,
        }
    }

    /// Creates a permanent engine error.
    pub fn permanent(reason: impl Into<String>) -> Self {
        Self {
            class: ContainerErrorClass::Permanent,
            reason: reason.into(),
            source: None,
        }
    }

    /// Creates a retryable error and preserves its source.
    pub fn retryable_from<E>(reason: impl Into<String>, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            class: ContainerErrorClass::Retryable,
            reason: reason.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Creates a permanent error and preserves its source.
    pub fn permanent_from<E>(reason: impl Into<String>, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            class: ContainerErrorClass::Permanent,
            reason: reason.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Returns the retry classification.
    pub fn class(&self) -> ContainerErrorClass {
        self.class
    }

    /// Returns the stable human-readable reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Debug for ContainerEngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContainerEngineError")
            .field("class", &self.class)
            .field("reason", &self.reason)
            .field("source", &self.source.as_ref().map(|_| "<source>"))
            .finish()
    }
}

impl fmt::Display for ContainerEngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl Error for ContainerEngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Information returned by an explicit engine probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerEngineInfo {
    name: String,
    version: String,
}

impl ContainerEngineInfo {
    /// Creates engine information.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }

    /// Returns the engine name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the engine version reported by the endpoint.
    pub fn version(&self) -> &str {
        &self.version
    }
}

/// Immutable task data passed to a container engine for one attempt.
#[derive(Debug, Clone)]
pub struct ContainerRequest {
    pub(super) attempt_id: String,
    pub(super) task_name: solti_model::TaskId,
    pub(super) generation: u64,
    pub(super) attempt: u32,
    pub(super) image: String,
    pub(super) command: Option<Vec<String>>,
    pub(super) args: Vec<String>,
    pub(super) env: std::collections::BTreeMap<String, String>,
    pub(super) process_policy: super::ContainerProcessPolicy,
}

impl ContainerRequest {
    /// Returns the unique process-local attempt identifier.
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    /// Returns the task resource name.
    pub fn task_name(&self) -> &solti_model::TaskId {
        &self.task_name
    }

    /// Returns the desired-state generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the Taskvisor attempt number.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the image reference from the task.
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Returns the optional image entrypoint override.
    ///
    /// An explicit empty override is normalized to `None`.
    pub fn command(&self) -> Option<&[String]> {
        self.command.as_deref()
    }

    /// Returns the image command arguments override.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Returns the merged task and runner environment.
    ///
    /// Runner values override task values.
    pub fn env(&self) -> &std::collections::BTreeMap<String, String> {
        &self.env
    }

    /// Returns the low-level process controls for this attempt.
    pub fn process_policy(&self) -> &super::ContainerProcessPolicy {
        &self.process_policy
    }
}

/// Exit status returned by a container engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerExitStatus {
    code: i32,
}

impl ContainerExitStatus {
    /// Creates a status from a process-style exit code.
    pub const fn new(code: i32) -> Self {
        Self { code }
    }

    /// Returns the process-style exit code.
    pub const fn code(self) -> i32 {
        self.code
    }

    /// Returns `true` for exit code zero.
    pub const fn success(self) -> bool {
        self.code == 0
    }
}

/// One engine-owned container attempt.
///
/// The attempt is created but not started.
/// Exit observation must already be armed when the engine returns it.
/// This prevents a short-lived process from exiting before `wait` is registered.
///
/// `terminate` and `cleanup` must be idempotent.
/// `cleanup` removes only resources owned by this attempt.
/// Cleanup may be repeated after a retryable error.
/// Completed cleanup steps must remain completed across calls.
#[async_trait]
pub trait ContainerAttempt: Send + 'static {
    /// Takes the captured stdout stream.
    fn take_stdout(&mut self) -> Option<ContainerOutput>;

    /// Takes the captured stderr stream.
    fn take_stderr(&mut self) -> Option<ContainerOutput>;

    /// Starts the already-created container process.
    async fn start(&mut self) -> Result<(), ContainerEngineError>;

    /// Waits for the container process to exit.
    async fn wait(&mut self) -> Result<ContainerExitStatus, ContainerEngineError>;

    /// Requests termination of the container process.
    async fn terminate(&mut self) -> Result<(), ContainerEngineError>;

    /// Removes attempt-scoped resources.
    async fn cleanup(&mut self) -> Result<(), ContainerEngineError>;
}

/// Engine used by [`ContainerRunner`](super::ContainerRunner).
///
/// Implementations own engine-specific setup and attempt resources.
/// They do not own the engine daemon or foreign resources.
#[async_trait]
pub trait ContainerEngine: Send + Sync + 'static {
    /// Checks endpoint availability and compatibility.
    ///
    /// The runner never calls this method implicitly.
    /// The final binary chooses whether probing is required at startup.
    async fn probe(&self) -> Result<ContainerEngineInfo, ContainerEngineError>;

    /// Resolves the image and creates one stopped attempt.
    ///
    /// A returned attempt must have exit observation armed before `start`.
    /// On failure, the engine must attempt to remove every confirmed owned resource.
    /// It must report incomplete or unconfirmed rollback as an error.
    async fn create_attempt(
        &self,
        request: ContainerRequest,
    ) -> Result<Box<dyn ContainerAttempt>, ContainerEngineError>;
}
