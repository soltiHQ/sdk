//! # Agent identity
//!
//! [`AgentId`] identifies one agent.
//! It accepts `[A-Za-z0-9._-]` and is limited to [`AGENT_ID_MAX_LEN`] bytes.

use super::validate_identity;
use crate::error::ModelError;

/// Maximum length of an `AgentId`.
pub const AGENT_ID_MAX_LEN: usize = 128;

arc_str_newtype! {
    /// Caller-provided identifier for a Solti agent.
    ///
    /// The model validates its format.
    /// The caller owns assignment and uniqueness.
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
    /// Validates the agent id.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::Invalid`] when the value is empty, too long, equal to `"."` or `".."`, or contains a byte outside `[A-Za-z0-9._-]`.
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
    fn exposes_string_identity_hashing_and_shared_clones() {
        use std::collections::HashSet;

        let id = AgentId::new("agent-a").unwrap();
        assert_eq!(id.as_str(), "agent-a");
        assert_eq!(format!("{id}"), "agent-a");
        assert_eq!(id, *"agent-a");

        let mut set = HashSet::new();
        set.insert(id.clone());
        set.insert(AgentId::new("agent-b").unwrap());
        set.insert(AgentId::new("agent-a").unwrap());
        assert_eq!(set.len(), 2);

        let cloned = id.clone();
        let a: Arc<str> = id.into_inner();
        let b: Arc<str> = cloned.into_inner();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn serde_is_transparent() {
        let id = AgentId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""550e8400-e29b-41d4-a716-446655440000""#);
        assert_eq!(serde_json::from_str::<AgentId>(&json).unwrap(), id);
    }

    #[test]
    fn validation_accepts_safe_values_and_rejects_unsafe_values() {
        for valid in [
            "550e8400-e29b-41d4-a716-446655440000",
            "worker-pod-7b9f4",
            "agent.eu-west-1.01",
        ] {
            AgentId::new(valid).unwrap();
        }
        for invalid in ["", "agent with space", "agent/path"] {
            assert!(AgentId::new(invalid).is_err(), "must reject {invalid:?}");
        }
    }
}
