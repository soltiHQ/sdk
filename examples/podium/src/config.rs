//! TOML configuration for the podium reference agent.
//!
//! One `transport` knob drives the agent's API server, the discovery client, and the
//! endpoint type advertised to podium. TLS and auth are independent on/off sections.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use solti::model::Token;
use solti::tls::{ClientTlsConfig, ServerTlsConfig, TlsIdentity, TrustRoots};

type BoxErr = Box<dyn std::error::Error>;

/// Top-level agent configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Transport for both the agent API and discovery: `http` or `grpc`.
    #[serde(default)]
    pub transport: Transport,
    pub agent: Agent,
    pub control_plane: ControlPlane,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub task: Task,
}

/// Wire transport selected at runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// HTTP/JSON (axum). Discovery on podium's HTTP port (default `:8082`).
    #[default]
    Http,
    /// gRPC (tonic). Discovery on podium's gRPC port (default `:50051`).
    Grpc,
}

/// Agent identity and addresses.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// Stable agent id reported to podium.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Address the agent binds its own TaskService on (podium connects here).
    pub listen: String,
    /// Address advertised to podium as the agent's reachable endpoint (host:port).
    pub advertise: String,
}

/// Control-plane (podium) discovery endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPlane {
    /// podium discovery endpoint, e.g. `http://127.0.0.1:8082` (HTTP) or `http://127.0.0.1:50051` (gRPC).
    pub endpoint: String,
    /// Heartbeat interval in milliseconds.
    #[serde(default = "default_heartbeat_ms")]
    pub heartbeat_ms: u64,
}

/// TLS / mTLS settings (whole section optional; default off).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// Enable TLS for both the agent API (server) and discovery (client).
    #[serde(default)]
    pub enabled: bool,
    /// Shared CA: trust root for podium, and (under mTLS) the client-cert verifier for the agent API.
    pub ca: Option<PathBuf>,
    /// Agent server certificate (podium → agent).
    pub cert: Option<PathBuf>,
    /// Agent server private key.
    pub key: Option<PathBuf>,
    /// Mutual TLS: require a client cert on the agent API and present one to podium.
    #[serde(default)]
    pub mtls: bool,
    /// Agent client certificate presented to podium (mTLS only).
    pub client_cert: Option<PathBuf>,
    /// Agent client private key (mTLS only).
    pub client_key: Option<PathBuf>,
}

/// Bearer-token auth (whole section optional; default off).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    /// Enable the shared bearer token on both inbound API and outbound discovery.
    #[serde(default)]
    pub enabled: bool,
    /// Token value (use `token_file` to load from disk instead).
    pub token: Option<String>,
    /// Path to a file holding the token (trimmed).
    pub token_file: Option<PathBuf>,
}

/// Optional local demo task submitted on startup.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_heartbeat_ms() -> u64 {
    10_000
}

impl Config {
    /// Read, parse and validate the config at `path`.
    pub fn load(path: &Path) -> Result<Self, BoxErr> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("reading config {}: {e}", path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).map_err(|e| format!("parsing config {}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), BoxErr> {
        if self.tls.enabled {
            if self.tls.ca.is_none() || self.tls.cert.is_none() || self.tls.key.is_none() {
                return Err("tls.enabled requires tls.ca, tls.cert and tls.key".into());
            }
            if self.tls.mtls && (self.tls.client_cert.is_none() || self.tls.client_key.is_none()) {
                return Err("tls.mtls requires tls.client_cert and tls.client_key".into());
            }
        }
        if self.auth.enabled && self.auth.token.is_none() && self.auth.token_file.is_none() {
            return Err("auth.enabled requires auth.token or auth.token_file".into());
        }
        if self.task.enabled && (self.task.name.is_empty() || self.task.command.is_empty()) {
            return Err("task.enabled requires task.name and task.command".into());
        }
        Ok(())
    }

    /// Shared bearer token (inbound API verification + outbound discovery), if auth is on.
    pub fn token(&self) -> Result<Option<Token>, BoxErr> {
        if !self.auth.enabled {
            return Ok(None);
        }
        if let Some(path) = &self.auth.token_file {
            Ok(Some(Token::from_file(path)?))
        } else if let Some(t) = &self.auth.token {
            Ok(Some(Token::new(t.clone())?))
        } else {
            Err("auth.enabled requires auth.token or auth.token_file".into())
        }
    }

    /// Server TLS for the agent's own API (podium connects to this), if TLS is on.
    pub fn server_tls(&self) -> Result<Option<ServerTlsConfig>, BoxErr> {
        if !self.tls.enabled {
            return Ok(None);
        }
        let cert = self.tls.cert.clone().ok_or("tls.cert is required")?;
        let key = self.tls.key.clone().ok_or("tls.key is required")?;
        let mut tls = ServerTlsConfig::new(TlsIdentity::from_pem_files(cert, key));
        if self.tls.mtls {
            let ca = self.tls.ca.clone().ok_or("tls.ca is required for mtls")?;
            tls = tls.require_client_auth(TrustRoots::from_pem_file(ca));
        }
        Ok(Some(tls))
    }

    /// Client TLS for discovery (the agent dials podium over TLS), if TLS is on.
    pub fn client_tls(&self) -> Result<Option<ClientTlsConfig>, BoxErr> {
        if !self.tls.enabled {
            return Ok(None);
        }
        let ca = self.tls.ca.clone().ok_or("tls.ca is required")?;
        let mut tls = ClientTlsConfig::new(TrustRoots::from_pem_file(ca));
        if self.tls.mtls {
            let cert = self
                .tls
                .client_cert
                .clone()
                .ok_or("tls.client_cert is required for mtls")?;
            let key = self
                .tls
                .client_key
                .clone()
                .ok_or("tls.client_key is required for mtls")?;
            tls = tls.with_identity(TlsIdentity::from_pem_files(cert, key));
        }
        Ok(Some(tls))
    }
}
