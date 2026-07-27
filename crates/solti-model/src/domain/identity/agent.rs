//! Agent identifier.
//!
//! [`AgentId`] identifies an agent instance in multi-agent deployments.

use super::validate_identity;
use crate::error::ModelError;

/// Maximum length of an `AgentId`.
pub const AGENT_ID_MAX_LEN: usize = 128;

arc_str_newtype! {
    /// Unique identifier for a Solti agent instance.
    ///
    /// Represents the identity of a running agent process.
    /// The caller is responsible for providing a meaningful ID, such as a UUID, hostname, or pod name.
    ///
    /// ```rust
    /// use solti_model::AgentId;
    ///
    /// // From a UUID
    /// let id = AgentId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    /// assert_eq!(id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
    ///
    /// // From a Kubernetes pod name
    /// let id = AgentId::new("worker-pod-7b9f4").unwrap();
    /// assert_eq!(format!("{id}"), "worker-pod-7b9f4");
    /// ```
    pub struct AgentId;
}

impl AgentId {
    /// Validate that the agent id is safe to use across the SDK and wire protocol.
    ///
    /// See `validate_identity` (module-private) for the exact rules.
    ///
    /// ## Errors
    ///
    /// - [`ModelError::Invalid`]: the id is empty, longer than [`AGENT_ID_MAX_LEN`],
    ///   equal to `"."` or `".."`, or contains a byte outside `[A-Za-z0-9._-]`.
    ///
    /// ## Example
    ///
    /// ```
    /// use solti_model::AgentId;
    ///
    /// assert!(AgentId::new("worker-pod-7b9f4").is_ok());
    /// assert!(AgentId::new("worker/pod").is_err());
    /// ```
    pub fn validate_format(&self) -> Result<(), ModelError> {
        validate_identity("agent_id", self.as_str(), AGENT_ID_MAX_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn agent_id_from_string() {
        let id = AgentId::new("my-agent-001").unwrap();
        assert_eq!(id.as_str(), "my-agent-001");
    }

    #[test]
    fn agent_id_display() {
        let id = AgentId::new("worker-pod-7b9f4").unwrap();
        assert_eq!(format!("{}", id), "worker-pod-7b9f4");
    }

    #[test]
    fn agent_id_serde_transparent() {
        let id = AgentId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""550e8400-e29b-41d4-a716-446655440000""#);

        let back: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn agent_id_hash_equality() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(AgentId::new("agent-a").unwrap());
        set.insert(AgentId::new("agent-b").unwrap());
        set.insert(AgentId::new("agent-a").unwrap());

        assert_eq!(set.len(), 2);
        assert!(set.contains(&AgentId::new("agent-a").unwrap()));
    }

    #[test]
    fn clone_is_cheap() {
        let id = AgentId::new("shared-agent").unwrap();
        let cloned = id.clone();
        let a: Arc<str> = id.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn partial_eq_with_str() {
        let id = AgentId::new("test-agent").unwrap();
        assert_eq!(id, *"test-agent");
    }

    #[test]
    fn into_inner() {
        let id = AgentId::new("owned").unwrap();
        let s: Arc<str> = id.into_inner();
        assert_eq!(&*s, "owned");
    }

    #[test]
    fn validate_format_accepts_valid() {
        AgentId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        AgentId::new("worker-pod-7b9f4").unwrap();
        AgentId::new("agent.eu-west-1.01").unwrap();
    }

    #[test]
    fn validate_format_rejects_invalid() {
        assert!(AgentId::new("").is_err());
        assert!(AgentId::new("agent with space").is_err());
        assert!(AgentId::new("agent/path").is_err());
    }
}
