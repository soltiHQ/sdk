//! Native containerd adapter configuration.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use containerd_client::tonic::metadata::{Ascii, MetadataValue};

use crate::container::ContainerEngineError;

const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Network namespace used by a containerd runner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerNetwork {
    /// Creates an isolated network namespace.
    ///
    /// No external interface, address, route, DNS, or NAT is configured.
    #[default]
    None,
    /// Uses the host network namespace.
    ///
    /// This mode must be selected explicitly by the final binary.
    /// It does not change OCI capabilities.
    /// The native adapter's base capability set includes `CAP_NET_RAW`.
    Host,
}

/// OCI platform selected for image pull and execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerPlatform {
    os: String,
    architecture: String,
    variant: String,
}

impl ContainerPlatform {
    /// Creates an OCI platform.
    pub fn new(
        os: impl Into<String>,
        architecture: impl Into<String>,
        variant: impl Into<String>,
    ) -> Self {
        let os = os.into();
        let architecture = architecture.into();
        let variant = variant.into();
        let (architecture, variant) = normalize_architecture(&architecture, &variant);
        Self {
            os: normalize_os(&os),
            architecture,
            variant,
        }
    }

    /// Returns the local Linux platform.
    pub fn host_linux() -> Self {
        Self::new("linux", std::env::consts::ARCH, "")
    }

    /// Returns the operating system.
    pub fn os(&self) -> &str {
        &self.os
    }

    /// Returns the OCI architecture.
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    /// Returns the architecture variant.
    pub fn variant(&self) -> &str {
        &self.variant
    }

    pub(super) fn as_containerd(&self) -> containerd_client::types::Platform {
        containerd_client::types::Platform {
            os: self.os.clone(),
            architecture: self.architecture.clone(),
            variant: self.variant.clone(),
            os_version: String::new(),
        }
    }
}

pub(super) fn normalize_os(os: &str) -> String {
    match os.to_ascii_lowercase().as_str() {
        "macos" => "darwin".to_owned(),
        os => os.to_owned(),
    }
}

pub(super) fn normalize_architecture(architecture: &str, variant: &str) -> (String, String) {
    let architecture = architecture.to_ascii_lowercase();
    let variant = variant.to_ascii_lowercase();
    match architecture.as_str() {
        "i386" | "x86" => ("386".to_owned(), String::new()),
        "x86_64" | "x86-64" | "amd64" => (
            "amd64".to_owned(),
            if variant == "v1" {
                String::new()
            } else {
                variant
            },
        ),
        "aarch64" | "arm64" => (
            "arm64".to_owned(),
            match variant.as_str() {
                "8" | "v8" | "v8.0" => String::new(),
                "9" | "9.0" | "v9.0" => "v9".to_owned(),
                _ => variant,
            },
        ),
        "armhf" => ("arm".to_owned(), "v7".to_owned()),
        "armel" => ("arm".to_owned(), "v6".to_owned()),
        "arm" => (
            "arm".to_owned(),
            match variant.as_str() {
                "" | "7" => "v7".to_owned(),
                "5" | "6" | "8" => format!("v{variant}"),
                _ => variant,
            },
        ),
        _ => (architecture, variant),
    }
}

/// Configuration for the native containerd 2.x adapter.
///
/// The socket, namespace, snapshotter, and OCI runtime are explicit.
/// The adapter never scans for daemons or starts containerd.
/// Control operations default to a 30-second deadline.
/// Image transfer defaults to a 10-minute deadline.
/// Cleanup retries share one 30-second window.
/// Workload wait has no deadline.
/// Zero-duration timeouts are rejected during connection validation.
#[derive(Debug, Clone)]
pub struct ContainerdConfig {
    socket: PathBuf,
    namespace: String,
    snapshotter: String,
    runtime: String,
    platform: ContainerPlatform,
    network: ContainerNetwork,
    registry_host_dir: Option<String>,
    io_root: PathBuf,
    control_timeout: Duration,
    transfer_timeout: Duration,
    cleanup_timeout: Duration,
}

impl ContainerdConfig {
    /// Creates containerd settings.
    pub fn new(
        socket: impl Into<PathBuf>,
        namespace: impl Into<String>,
        snapshotter: impl Into<String>,
        runtime: impl Into<String>,
    ) -> Self {
        Self {
            socket: socket.into(),
            namespace: namespace.into(),
            snapshotter: snapshotter.into(),
            runtime: runtime.into(),
            platform: ContainerPlatform::host_linux(),
            network: ContainerNetwork::None,
            registry_host_dir: None,
            io_root: std::env::temp_dir(),
            control_timeout: DEFAULT_CONTROL_TIMEOUT,
            transfer_timeout: DEFAULT_TRANSFER_TIMEOUT,
            cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
        }
    }

    /// Selects the OCI image platform.
    pub fn with_platform(mut self, platform: ContainerPlatform) -> Self {
        self.platform = platform;
        self
    }

    /// Selects the network namespace mode.
    pub fn with_network(mut self, network: ContainerNetwork) -> Self {
        self.network = network;
        self
    }

    /// Sets a containerd registry hosts directory.
    ///
    /// The path is interpreted by the containerd daemon.
    pub fn with_registry_host_dir(mut self, path: impl Into<String>) -> Self {
        self.registry_host_dir = Some(path.into());
        self
    }

    /// Sets the parent directory for private attempt I/O directories.
    ///
    /// The directory must be visible at the same path to this process and containerd.
    /// Writable shared ancestors must use the sticky bit.
    pub fn with_io_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.io_root = path.into();
        self
    }

    /// Sets the client-side deadline for connect, probe, metadata, and lifecycle operations.
    ///
    /// This deadline does not apply to image transfer or workload wait.
    pub fn with_control_timeout(mut self, timeout: Duration) -> Self {
        self.control_timeout = timeout;
        self
    }

    /// Sets the client-side deadline for image pull and unpack.
    pub fn with_transfer_timeout(mut self, timeout: Duration) -> Self {
        self.transfer_timeout = timeout;
        self
    }

    /// Sets the total retry window for attempt cleanup.
    pub fn with_cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.cleanup_timeout = timeout;
        self
    }

    /// Returns the configured containerd socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Returns the containerd namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Returns the snapshotter name.
    pub fn snapshotter(&self) -> &str {
        &self.snapshotter
    }

    /// Returns the OCI runtime name.
    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    /// Returns the selected OCI platform.
    pub fn platform(&self) -> &ContainerPlatform {
        &self.platform
    }

    /// Returns the selected network mode.
    pub fn network(&self) -> ContainerNetwork {
        self.network
    }

    /// Returns the registry hosts directory passed to containerd.
    pub fn registry_host_dir(&self) -> Option<&str> {
        self.registry_host_dir.as_deref()
    }

    /// Returns the parent directory for attempt I/O.
    pub fn io_root(&self) -> &Path {
        &self.io_root
    }

    /// Returns the connect, probe, metadata, and lifecycle deadline.
    pub fn control_timeout(&self) -> Duration {
        self.control_timeout
    }

    /// Returns the image pull and unpack deadline.
    pub fn transfer_timeout(&self) -> Duration {
        self.transfer_timeout
    }

    /// Returns the total cleanup retry window.
    pub fn cleanup_timeout(&self) -> Duration {
        self.cleanup_timeout
    }

    pub(super) fn validate(&self) -> Result<MetadataValue<Ascii>, ContainerEngineError> {
        if !self.socket.is_absolute() {
            return Err(ContainerEngineError::permanent(
                "containerd socket path must be absolute",
            ));
        }
        validate_identifier("namespace", &self.namespace)?;
        let namespace = self.namespace.parse().map_err(|error| {
            ContainerEngineError::permanent_from("invalid containerd namespace", error)
        })?;
        validate_identifier("snapshotter", &self.snapshotter)?;
        for (field, value) in [
            ("runtime", self.runtime.as_str()),
            ("platform.os", self.platform.os()),
            ("platform.architecture", self.platform.architecture()),
        ] {
            if value.trim().is_empty() || value.contains('\0') {
                return Err(ContainerEngineError::permanent(format!(
                    "containerd {field} must be a non-empty string without NUL"
                )));
            }
        }
        if !self.platform.os.eq_ignore_ascii_case("linux") {
            return Err(ContainerEngineError::permanent(
                "native containerd execution supports Linux images only",
            ));
        }
        if self.platform.variant.contains('\0') {
            return Err(ContainerEngineError::permanent(
                "containerd platform.variant cannot contain NUL",
            ));
        }
        if self
            .registry_host_dir
            .as_deref()
            .is_some_and(|path| path.is_empty() || path.contains('\0'))
        {
            return Err(ContainerEngineError::permanent(
                "containerd registry host directory must be non-empty and contain no NUL",
            ));
        }
        if !self.io_root.is_absolute() {
            return Err(ContainerEngineError::permanent(
                "containerd I/O root must be absolute",
            ));
        }
        for (field, timeout) in [
            ("control timeout", self.control_timeout),
            ("transfer timeout", self.transfer_timeout),
            ("cleanup timeout", self.cleanup_timeout),
        ] {
            if timeout.is_zero() {
                return Err(ContainerEngineError::permanent(format!(
                    "containerd {field} cannot be zero"
                )));
            }
        }
        Ok(namespace)
    }
}

pub(super) fn validate_identifier(field: &str, value: &str) -> Result<(), ContainerEngineError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 76
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !bytes
            .windows(2)
            .any(|pair| !pair[0].is_ascii_alphanumeric() && !pair[1].is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        Err(ContainerEngineError::permanent(format!(
            "containerd {field} must be a 1..=76 byte containerd identifier"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ContainerdConfig {
        ContainerdConfig::new(
            "/run/containerd/containerd.sock",
            "solti",
            "overlayfs",
            "io.containerd.runc.v2",
        )
    }

    #[test]
    fn defaults_are_explicit_and_isolated() {
        let config = config();
        assert_eq!(config.network(), ContainerNetwork::None);
        assert_eq!(config.platform().os(), "linux");
        assert_eq!(config.control_timeout(), Duration::from_secs(30));
        assert_eq!(config.transfer_timeout(), Duration::from_secs(10 * 60));
        assert_eq!(config.cleanup_timeout(), Duration::from_secs(30));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn platforms_use_containerd_normalization() {
        let amd64 = ContainerPlatform::new("LINUX", "x86_64", "v1");
        assert_eq!(
            (amd64.os(), amd64.architecture(), amd64.variant()),
            ("linux", "amd64", "")
        );

        let arm64 = ContainerPlatform::new("linux", "aarch64", "8");
        assert_eq!((arm64.architecture(), arm64.variant()), ("arm64", ""));

        let arm = ContainerPlatform::new("linux", "armhf", "");
        assert_eq!((arm.architecture(), arm.variant()), ("arm", "v7"));

        let x86 = ContainerPlatform::new("linux", "x86", "");
        assert_eq!((x86.architecture(), x86.variant()), ("386", ""));
    }

    #[test]
    fn relative_paths_and_invalid_metadata_are_rejected() {
        assert!(
            ContainerdConfig::new("relative.sock", "solti", "overlayfs", "runc")
                .validate()
                .is_err()
        );
        assert!(
            ContainerdConfig::new("/run/containerd.sock", "bad\nvalue", "overlayfs", "runc")
                .validate()
                .is_err()
        );
        assert!(
            ContainerdConfig::new("/run/containerd.sock", "bad..value", "overlayfs", "runc")
                .validate()
                .is_err()
        );
        assert!(
            ContainerdConfig::new("/run/containerd.sock", "solti", "bad_snapshotter", "runc")
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn zero_timeouts_are_rejected() {
        assert!(
            config()
                .with_control_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            config()
                .with_transfer_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
        assert!(
            config()
                .with_cleanup_timeout(Duration::ZERO)
                .validate()
                .is_err()
        );
    }
}
