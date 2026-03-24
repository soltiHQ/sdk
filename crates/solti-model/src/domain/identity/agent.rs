use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Unique identifier for a solti agent instance.
///
/// Represents the identity of a running agent process.
/// The caller is responsible for providing a meaningful ID (e.g. UUID, hostname, pod name).
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(Arc<str>);

impl AgentId {
    /// Create a new agent ID from a string.
    #[inline]
    pub fn new(id: &str) -> Self {
        Self(Arc::from(id))
    }

    /// Get the agent ID as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert into the underlying `Arc<str>`.
    #[inline]
    pub fn into_inner(self) -> Arc<str> {
        self.0
    }
}

impl From<String> for AgentId {
    #[inline]
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl From<&str> for AgentId {
    #[inline]
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<Arc<str>> for AgentId {
    #[inline]
    fn from(s: Arc<str>) -> Self {
        Self(s)
    }
}

impl AsRef<str> for AgentId {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for AgentId {
    #[inline]
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for AgentId {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for AgentId {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for AgentId {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

impl Serialize for AgentId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|s| Self(Arc::from(s)))
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
        assert!(Arc::ptr_eq(&id.0, &cloned.0));
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
}
