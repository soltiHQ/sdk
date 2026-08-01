//! # Linux process credentials

/// Exact Linux credentials applied to a process.
///
/// Supplementary groups are replaced by the provided list.
/// Empty supplementary groups clear the inherited list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCredentials {
    /// User ID.
    pub uid: u32,
    /// Group ID.
    pub gid: u32,
    /// Exact supplementary group list.
    pub supplementary_groups: Vec<u32>,
}

impl ProcessCredentials {
    /// Creates credentials with no supplementary groups.
    pub fn new(uid: u32, gid: u32) -> Self {
        Self {
            uid,
            gid,
            supplementary_groups: Vec::new(),
        }
    }

    /// Replaces the supplementary group list.
    pub fn with_supplementary_groups(mut self, supplementary_groups: impl Into<Vec<u32>>) -> Self {
        self.supplementary_groups = supplementary_groups.into();
        self
    }
}

pub(crate) fn validate_credentials(credentials: &ProcessCredentials) -> Result<(), String> {
    if credentials.uid == u32::MAX {
        return Err("security.credentials.uid cannot be the unchanged-ID sentinel".into());
    }
    if credentials.gid == u32::MAX {
        return Err("security.credentials.gid cannot be the unchanged-ID sentinel".into());
    }
    if credentials.supplementary_groups.contains(&u32::MAX) {
        return Err(
            "security.credentials.supplementary_groups cannot contain the unchanged-ID sentinel"
                .into(),
        );
    }
    Ok(())
}
