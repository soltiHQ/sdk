//! # Task API access control
//!
//! Authentication turns one bearer credential into an [`ApiIdentity`].
//! Authorization decides whether that identity can perform one [`TaskOperation`].
//!
//! The contracts are transport-neutral and storage-neutral. They do not define
//! users, roles, tenants, or RBAC rules.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
#[cfg(any(feature = "http", test))]
use solti_model::Token;
use solti_model::{TaskId, TaskManifest};

use crate::{ApiError, Transport};

/// Identity produced by an [`ApiAuthenticator`].
///
/// A subject is optional because the built-in static bearer token proves that the caller knows one shared secret but does not identify an individual user.
/// Attributes are application-owned.
/// Solti does not interpret them as roles, permissions, or tenant claims.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiIdentity {
    subject: Option<String>,
    attributes: BTreeMap<String, Vec<String>>,
}

impl ApiIdentity {
    /// Creates an authenticated identity without an individual subject.
    pub fn authenticated() -> Self {
        Self::default()
    }

    /// Creates an authenticated identity for one application-defined subject.
    pub fn for_subject(subject: impl Into<String>) -> Self {
        Self {
            subject: Some(subject.into()),
            attributes: BTreeMap::new(),
        }
    }

    /// Adds one application-defined attribute value.
    pub fn with_attribute(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes
            .entry(name.into())
            .or_default()
            .push(value.into());
        self
    }

    /// Returns the application-defined subject, when authentication produced one.
    pub fn subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Returns every value stored for one application-defined attribute.
    pub fn attribute(&self, name: &str) -> Option<&[String]> {
        self.attributes.get(name).map(Vec::as_slice)
    }

    /// Returns all application-defined attributes.
    pub fn attributes(&self) -> &BTreeMap<String, Vec<String>> {
        &self.attributes
    }
}

/// Bearer authentication input shared by HTTP and gRPC.
///
/// The credential is borrowed from the current request. Implementations should
/// not retain or log it.
#[derive(Clone, Copy)]
pub struct AuthenticationRequest<'a> {
    transport: Transport,
    bearer_credential: Option<&'a str>,
}

impl<'a> AuthenticationRequest<'a> {
    /// Creates an authentication request.
    pub fn new(transport: Transport, bearer_credential: Option<&'a str>) -> Self {
        Self {
            transport,
            bearer_credential,
        }
    }

    /// Returns the serving transport.
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// Returns the value after the `Bearer` scheme, when present and valid UTF-8.
    pub fn bearer_credential(&self) -> Option<&'a str> {
        self.bearer_credential
    }
}

/// Turns one bearer credential into an application identity.
///
/// Implementations may validate JWTs, call an identity service, or use another application-owned mechanism.
/// Reject missing or invalid credentials with [`ApiError::Unauthenticated`].
#[async_trait]
pub trait ApiAuthenticator: Send + Sync + 'static {
    /// Authenticates one request.
    async fn authenticate(
        &self,
        request: AuthenticationRequest<'_>,
    ) -> Result<ApiIdentity, ApiError>;
}

/// Shared authenticator handle accepted by both transport builders.
pub type ApiAuthenticatorHandle = Arc<dyn ApiAuthenticator>;

/// Task API operation checked by an [`ApiAuthorizer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TaskOperation {
    /// Create a task from desired state.
    Create,
    /// Create or update desired state.
    Apply,
    /// Read one task.
    Get,
    /// List the task collection.
    List,
    /// Watch the task collection.
    Watch,
    /// Read retained runs for one task.
    ListRuns,
    /// Delete one task.
    Delete,
    /// Open one live output stream.
    StreamLogs,
}

impl TaskOperation {
    /// Returns the stable operation label.
    pub fn as_label(self) -> &'static str {
        match self {
            TaskOperation::Create => "create",
            TaskOperation::Apply => "apply",
            TaskOperation::Get => "get",
            TaskOperation::List => "list",
            TaskOperation::Watch => "watch",
            TaskOperation::ListRuns => "list_runs",
            TaskOperation::Delete => "delete",
            TaskOperation::StreamLogs => "stream_logs",
        }
    }
}

/// Resource target checked by an [`ApiAuthorizer`].
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum TaskTarget<'a> {
    /// The complete Task collection, used by list and watch.
    Collection,
    /// One task addressed by name.
    Task(&'a TaskId),
    /// Desired state supplied to create or apply.
    Manifest(&'a TaskManifest),
}

/// Authorization input shared by HTTP and gRPC.
#[derive(Clone, Copy)]
pub struct AuthorizationRequest<'a> {
    identity: Option<&'a ApiIdentity>,
    operation: TaskOperation,
    target: TaskTarget<'a>,
}

impl<'a> AuthorizationRequest<'a> {
    /// Creates an authorization request.
    pub fn new(
        identity: Option<&'a ApiIdentity>,
        operation: TaskOperation,
        target: TaskTarget<'a>,
    ) -> Self {
        Self {
            identity,
            operation,
            target,
        }
    }

    /// Returns the authenticated identity, when one is available.
    pub fn identity(&self) -> Option<&'a ApiIdentity> {
        self.identity
    }

    /// Returns the requested operation.
    pub fn operation(&self) -> TaskOperation {
        self.operation
    }

    /// Returns the requested resource target.
    pub fn target(&self) -> TaskTarget<'a> {
        self.target
    }
}

/// Allows or denies one validated Task API operation.
///
/// Return [`ApiError::Forbidden`] for a normal policy denial.
/// Other errors can report an unavailable or failed policy backend.
/// The hook runs before the [`crate::ApiHandler`] operation starts.
#[async_trait]
pub trait ApiAuthorizer: Send + Sync + 'static {
    /// Authorizes one request.
    async fn authorize(&self, request: AuthorizationRequest<'_>) -> Result<(), ApiError>;
}

/// Shared authorizer handle accepted by both transport builders.
pub type ApiAuthorizerHandle = Arc<dyn ApiAuthorizer>;

#[cfg(any(feature = "http", test))]
pub(crate) struct StaticBearerAuthenticator {
    expected: Token,
}

#[cfg(any(feature = "http", test))]
impl StaticBearerAuthenticator {
    pub(crate) fn new(expected: Token) -> Self {
        Self { expected }
    }
}

#[cfg(any(feature = "http", test))]
#[async_trait]
impl ApiAuthenticator for StaticBearerAuthenticator {
    async fn authenticate(
        &self,
        request: AuthenticationRequest<'_>,
    ) -> Result<ApiIdentity, ApiError> {
        let valid = request
            .bearer_credential()
            .map(|presented| self.expected.verify(presented))
            .unwrap_or(false);
        if valid {
            Ok(ApiIdentity::authenticated())
        } else {
            Err(ApiError::Unauthenticated(
                "missing or invalid bearer token".into(),
            ))
        }
    }
}

/// Extracts a bearer credential.
///
/// The scheme comparison is case-insensitive.
/// The value after the first space is returned without trimming.
#[cfg(any(feature = "grpc", feature = "http", test))]
pub(crate) fn bearer_value(header: &str) -> Option<&str> {
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_value_accepts_scheme_case_insensitively() {
        assert_eq!(bearer_value("Bearer tok"), Some("tok"));
        assert_eq!(bearer_value("bearer tok"), Some("tok"));
        assert_eq!(bearer_value("BEARER tok"), Some("tok"));
        assert_eq!(bearer_value("BeArEr tok"), Some("tok"));
        assert_eq!(bearer_value("Bearer a b"), Some("a b"));
        assert_eq!(bearer_value("Basic tok"), None);
        assert_eq!(bearer_value("tok"), None);
        assert_eq!(bearer_value(""), None);
    }

    #[test]
    fn identity_keeps_subject_and_application_attributes() {
        let identity = ApiIdentity::for_subject("user-1")
            .with_attribute("team", "runtime")
            .with_attribute("team", "operations");

        assert_eq!(identity.subject(), Some("user-1"));
        assert_eq!(
            identity.attribute("team"),
            Some(["runtime".to_string(), "operations".to_string()].as_slice())
        );
    }
}
