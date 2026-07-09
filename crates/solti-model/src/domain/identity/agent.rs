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
    /// let id = AgentId::new("550e8400-e29b-41d4-a716-446655440000");
    /// assert_eq!(id.as_str(), "550e8400-e29b-41d4-a716-446655440000");
    ///
    /// // From a Kubernetes pod name
    /// let id: AgentId = "worker-pod-7b9f4".into();
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
    /// assert!(AgentId::new("worker-pod-7b9f4").validate_format().is_ok());
    /// assert!(AgentId::new("worker/pod").validate_format().is_err());
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
        let id = AgentId::from("my-agent-001");
        assert_eq!(id.as_str(), "my-agent-001");
    }

    #[test]
    fn agent_id_display() {
        let id = AgentId::new("worker-pod-7b9f4");
        assert_eq!(format!("{}", id), "worker-pod-7b9f4");
    }

    #[test]
    fn agent_id_serde_transparent() {
        let id = AgentId::from("550e8400-e29b-41d4-a716-446655440000");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""550e8400-e29b-41d4-a716-446655440000""#);

        let back: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn agent_id_hash_equality() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(AgentId::from("agent-a"));
        set.insert(AgentId::from("agent-b"));
        set.insert(AgentId::from("agent-a"));

        assert_eq!(set.len(), 2);
        assert!(set.contains(&AgentId::from("agent-a")));
    }

    #[test]
    fn clone_is_cheap() {
        let id = AgentId::new("shared-agent");
        let cloned = id.clone();
        let a: Arc<str> = id.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn partial_eq_with_str() {
        let id = AgentId::new("test-agent");
        assert_eq!(id, *"test-agent");
    }

    #[test]
    fn into_inner() {
        let id = AgentId::new("owned");
        let s: Arc<str> = id.into_inner();
        assert_eq!(&*s, "owned");
    }

    #[test]
    fn validate_format_accepts_valid() {
        AgentId::new("550e8400-e29b-41d4-a716-446655440000")
            .validate_format()
            .unwrap();
        AgentId::new("worker-pod-7b9f4").validate_format().unwrap();
        AgentId::new("agent.eu-west-1.01")
            .validate_format()
            .unwrap();
    }

    #[test]
    fn validate_format_rejects_invalid() {
        assert!(AgentId::new("").validate_format().is_err());
        assert!(AgentId::new("agent with space").validate_format().is_err());
        assert!(AgentId::new("agent/path").validate_format().is_err());
    }
}
