//! Typed access to a routed workload body.

use std::fmt;

/// A Rust type that decodes the `spec` of one workload type.
///
/// The GVK a body type owns is the key the runner registers and Podium
/// validates against.
pub trait WorkloadBody: serde::de::DeserializeOwned + 'static {
    /// Workload API group and version.
    const API_VERSION: &'static str;

    /// Workload kind.
    const KIND: &'static str;

    /// Checks what the schema cannot express.
    ///
    /// A derived [`WorkloadDecl`](crate::WorkloadDecl) runs this inside its admission check,
    /// under the task's write lock and a state-write admission permit: keep it fast and
    /// non-blocking.
    ///
    /// # Errors
    ///
    /// Returns the reason the decoded body is refused.
    fn validate(&self) -> Result<(), String> {
        Ok(())
    }
}

/// Why a routed workload could not be decoded into a [`WorkloadBody`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WorkloadBodyErrorKind {
    /// The workload belongs to another type.
    TypeMismatch {
        /// API group and version the body type expects.
        expected_api_version: String,
        /// Kind the body type expects.
        expected_kind: String,
    },
    /// `spec` does not deserialize into the body type.
    Decode {
        /// Path of the offending field below `spec`, `.`-separated.
        path: String,
    },
    /// The body decoded but its own `validate` refused it.
    Invalid,
}

/// Failure of [`Workload::decode`](crate::Workload::decode).
///
/// Display names the workload GVK once: `workload <apiVersion>/<kind>: spec.<path>: <message>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadBodyError {
    api_version: String,
    kind: String,
    reason: WorkloadBodyErrorKind,
    message: String,
}

impl WorkloadBodyError {
    pub(super) fn new(
        api_version: &str,
        kind: &str,
        reason: WorkloadBodyErrorKind,
        message: String,
    ) -> Self {
        Self {
            api_version: api_version.to_owned(),
            kind: kind.to_owned(),
            reason,
            message,
        }
    }

    /// Failure category.
    #[inline]
    pub fn kind(&self) -> &WorkloadBodyErrorKind {
        &self.reason
    }

    /// Human-readable reason, without the GVK and path prefix.
    #[inline]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl WorkloadBodyError {
    /// The reason without the GVK: `spec.<path>: <message>`, `spec: <message>` at the root,
    /// or the bare message when no field is at fault.
    ///
    /// A caller that names the GVK itself uses this instead of [`Display`](fmt::Display).
    pub fn detail(&self) -> String {
        match &self.reason {
            WorkloadBodyErrorKind::TypeMismatch { .. } | WorkloadBodyErrorKind::Invalid => {
                self.message.clone()
            }
            WorkloadBodyErrorKind::Decode { path } if path.is_empty() => {
                format!("spec: {}", self.message)
            }
            WorkloadBodyErrorKind::Decode { path } => format!("spec.{path}: {}", self.message),
        }
    }
}

impl fmt::Display for WorkloadBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "workload {}/{}: {}",
            self.api_version,
            self.kind,
            self.detail()
        )
    }
}

impl std::error::Error for WorkloadBodyError {}
