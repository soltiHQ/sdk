//! Container process policy.

use crate::isolation::{
    CgroupLimits, LinuxCapability, ProcessCredentials, RlimitConfig, SeccompPolicy,
    validate_cgroup_limits, validate_credentials, validate_umask,
};

/// Low-level process controls applied by a container engine.
///
/// An empty policy preserves the values produced from the image and engine defaults.
/// Configured capabilities replace all five OCI capability sets.
#[derive(Debug, Clone, Default)]
pub struct ContainerProcessPolicy {
    rlimits: Option<RlimitConfig>,
    resources: Option<CgroupLimits>,
    credentials: Option<ProcessCredentials>,
    capabilities: Option<Vec<LinuxCapability>>,
    no_new_privileges: bool,
    umask: Option<u32>,
    seccomp: SeccompPolicy,
}

impl ContainerProcessPolicy {
    /// Creates an empty policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets POSIX process limits.
    pub fn with_rlimits(mut self, rlimits: RlimitConfig) -> Self {
        self.rlimits = Some(rlimits);
        self
    }

    /// Sets Linux CPU, memory, and process limits.
    pub fn with_resources(mut self, resources: CgroupLimits) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Sets exact numeric credentials for the container process.
    ///
    /// The native containerd adapter does not create a user namespace.
    /// It interprets these IDs in the host user namespace.
    ///
    /// This setting requires an explicit [`Self::with_no_new_privileges`] call with `true`.
    pub fn with_credentials(mut self, credentials: ProcessCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Replaces every OCI capability set with the provided capabilities.
    ///
    /// An empty list drops all capabilities.
    /// This setting requires an explicit [`Self::with_no_new_privileges`] call with `true`.
    pub fn with_capabilities(mut self, capabilities: impl Into<Vec<LinuxCapability>>) -> Self {
        self.capabilities = Some(capabilities.into());
        self
    }

    /// Enables or disables the explicit no-new-privileges request.
    ///
    /// `false` never clears a value already present in the base OCI specification.
    pub fn with_no_new_privileges(mut self, enabled: bool) -> Self {
        self.no_new_privileges = enabled;
        self
    }

    /// Sets the process file creation mask.
    pub fn with_umask(mut self, umask: u32) -> Self {
        self.umask = Some(umask);
        self
    }

    /// Sets the seccomp intent.
    pub fn with_seccomp(mut self, seccomp: SeccompPolicy) -> Self {
        self.seccomp = seccomp;
        self
    }

    /// Returns configured POSIX process limits.
    pub fn rlimits(&self) -> Option<&RlimitConfig> {
        self.rlimits.as_ref()
    }

    /// Returns configured Linux resource limits.
    pub fn resources(&self) -> Option<&CgroupLimits> {
        self.resources.as_ref()
    }

    /// Returns configured numeric credentials.
    pub fn credentials(&self) -> Option<&ProcessCredentials> {
        self.credentials.as_ref()
    }

    /// Returns the exact capability replacement.
    ///
    /// `None` preserves the base OCI capability sets.
    /// An empty slice drops every capability.
    pub fn capabilities(&self) -> Option<&[LinuxCapability]> {
        self.capabilities.as_deref()
    }

    /// Returns the explicit no-new-privileges request.
    pub fn no_new_privileges(&self) -> bool {
        self.no_new_privileges
    }

    /// Returns the configured file creation mask.
    pub fn umask(&self) -> Option<u32> {
        self.umask
    }

    /// Returns the configured seccomp intent.
    pub fn seccomp(&self) -> &SeccompPolicy {
        &self.seccomp
    }

    /// Returns `true` when the policy changes no base value.
    pub fn is_empty(&self) -> bool {
        self.rlimits.as_ref().is_none_or(RlimitConfig::is_empty)
            && self.resources.is_none()
            && self.credentials.is_none()
            && self.capabilities.is_none()
            && !self.no_new_privileges
            && self.umask.is_none()
            && self.seccomp == SeccompPolicy::Disabled
    }

    #[cfg(feature = "containerd")]
    pub(crate) fn effective_no_new_privileges(&self) -> bool {
        self.no_new_privileges || self.seccomp != SeccompPolicy::Disabled
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(resources) = &self.resources {
            validate_cgroup_limits(resources)?;
        }
        if let Some(credentials) = &self.credentials {
            validate_credentials(credentials)?;
        }
        if let Some(umask) = self.umask {
            validate_umask(umask)?;
        }
        if self.credentials.is_some() && !self.no_new_privileges {
            return Err("container.credentials requires container.no_new_privileges = true".into());
        }
        if self.capabilities.is_some() && !self.no_new_privileges {
            return Err(
                "container.capabilities requires container.no_new_privileges = true".into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_changes_nothing() {
        assert!(ContainerProcessPolicy::new().is_empty());
    }

    #[test]
    fn exact_identity_requires_explicit_no_new_privileges() {
        let policy =
            ContainerProcessPolicy::new().with_credentials(ProcessCredentials::new(1000, 1000));

        assert_eq!(
            policy.validate().unwrap_err(),
            "container.credentials requires container.no_new_privileges = true"
        );
        assert!(policy.with_no_new_privileges(true).validate().is_ok());
    }

    #[test]
    fn capability_replacement_requires_explicit_no_new_privileges() {
        let policy = ContainerProcessPolicy::new().with_capabilities([]);

        assert_eq!(
            policy.validate().unwrap_err(),
            "container.capabilities requires container.no_new_privileges = true"
        );
        assert!(policy.with_no_new_privileges(true).validate().is_ok());
    }

    #[test]
    #[cfg(feature = "containerd")]
    fn seccomp_enables_effective_no_new_privileges() {
        let policy = ContainerProcessPolicy::new().with_seccomp(SeccompPolicy::DenyHostControl);

        assert!(policy.effective_no_new_privileges());
        assert!(policy.validate().is_ok());
    }
}
