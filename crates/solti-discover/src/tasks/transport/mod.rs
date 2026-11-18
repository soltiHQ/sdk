//! # Discovery transport
//!
//! [`TransportAdapter`] owns exactly one selected transport.
//!
//! ```text
//! DiscoverConfig.control_plane.transport
//!                  ▼
//!          TransportAdapter
//!             ├──► HTTP adapter
//!             └──► gRPC adapter
//!                  ▼
//!             SyncResponse
//!                  │ validate_response
//!                  ▼
//!   accepted SyncResponse or DiscoverError
//! ```
//!
//! Adapter construction validates transport-specific URI and authentication data.
//! A bearer token requires TLS unless the config explicitly opts into plaintext transport.
//! The transport client is reused across task attempts.
//!
//! `encoded_len` measures a request with the adapter's own encoder: protobuf for
//! gRPC, the exact JSON body for HTTP.

#[cfg(feature = "grpc")]
mod grpc;
#[cfg(feature = "http")]
mod http;

use crate::config::{DiscoverConfig, DiscoveryTransport};
use crate::errors::DiscoverError;
use crate::proto::{SyncRequest, SyncResponse};

#[cfg(feature = "grpc")]
use grpc::GrpcAdapter;
#[cfg(feature = "http")]
use http::HttpAdapter;

/// Selected transport for one discovery task.
pub(super) enum TransportAdapter {
    #[cfg(feature = "grpc")]
    Grpc(Box<GrpcAdapter>),
    #[cfg(feature = "http")]
    Http(HttpAdapter),
}

impl TransportAdapter {
    /// Creates only the adapter selected by the config.
    pub(super) fn from_config(config: &DiscoverConfig) -> Result<Self, DiscoverError> {
        match config.control_plane.transport {
            #[cfg(feature = "grpc")]
            DiscoveryTransport::Grpc => GrpcAdapter::new(config).map(Box::new).map(Self::Grpc),
            #[cfg(feature = "http")]
            DiscoveryTransport::Http => HttpAdapter::new(config).map(Self::Http),
        }
    }

    /// Sends one request through the selected adapter.
    ///
    /// Returns the accepted response, `want_schemas` included.
    pub(super) async fn sync(&self, request: SyncRequest) -> Result<SyncResponse, DiscoverError> {
        match self {
            #[cfg(feature = "grpc")]
            Self::Grpc(adapter) => adapter.sync(request).await,
            #[cfg(feature = "http")]
            Self::Http(adapter) => adapter.sync(request).await,
        }
    }

    /// Returns the size of `request` as the selected adapter encodes it.
    ///
    /// This is the size the control plane's request limit counts.
    pub(super) fn encoded_len(&self, request: &SyncRequest) -> usize {
        match self {
            #[cfg(feature = "grpc")]
            Self::Grpc(adapter) => adapter.encoded_len(request),
            #[cfg(feature = "http")]
            Self::Http(adapter) => adapter.encoded_len(request),
        }
    }

    /// Returns whether the selected endpoint uses TLS.
    pub(super) fn is_secure(&self) -> bool {
        match self {
            #[cfg(feature = "grpc")]
            Self::Grpc(adapter) => adapter.is_secure(),
            #[cfg(feature = "http")]
            Self::Http(adapter) => adapter.is_secure(),
        }
    }
}

/// Enforces the bearer credential transport policy after URI validation.
fn validate_token_transport(config: &DiscoverConfig, secure: bool) -> Result<(), DiscoverError> {
    if config.token.is_some() && !secure && !config.allow_insecure_token_transport {
        return Err(DiscoverError::InvalidConfig(
            "bearer token over plaintext discovery is disabled; use an https control-plane endpoint or explicitly call allow_insecure_token_transport() for development or loopback use"
                .into(),
        ));
    }

    Ok(())
}

/// Returns an accepted response and converts `success = false` into
/// [`DiscoverError::Rejected`].
///
/// The rejection reason remains untrusted server text.
fn validate_response(response: SyncResponse) -> Result<SyncResponse, DiscoverError> {
    if response.success {
        return Ok(response);
    }

    let reason = if response.reason.is_empty() {
        "control plane returned success=false".to_string()
    } else {
        response.reason
    };
    let retry_after_s = (response.retry_after_s > 0).then_some(response.retry_after_s);

    Err(DiscoverError::Rejected {
        reason,
        retry_after_s,
    })
}

/// A control plane for tests, with Podium's side of the schema exchange.
///
/// [`FakeControlPlane`](fake::FakeControlPlane) holds the state; the HTTP and
/// gRPC fakes serve it on a loopback port.
#[cfg(test)]
pub(in crate::tasks) mod fake {
    use std::collections::{BTreeMap, BTreeSet};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sha2::{Digest as _, Sha256};
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    use crate::config::DiscoveryTransport;
    use crate::proto::SyncRequest;

    /// Podium's cap on one discovery request: the HTTP body or the gRPC message.
    pub(in crate::tasks) const PODIUM_MAX_SYNC_BYTES: usize = 262_144;

    /// How the fake answered one request.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::tasks) enum Outcome {
        /// Accepted; the response wants these fingerprints.
        Accepted(Vec<String>),
        /// Refused for its size: HTTP 413, gRPC `ResourceExhausted`.
        TooLarge,
        /// Failed on purpose: HTTP 503, gRPC `Unavailable`.
        Unavailable,
    }

    /// One request the fake received.
    #[derive(Clone, Debug)]
    pub(in crate::tasks) struct Seen {
        /// Its encoded size: the HTTP body or the gRPC message.
        pub(in crate::tasks) bytes: usize,
        /// The fingerprints of the schemas it carried, in order.
        pub(in crate::tasks) schemas: Vec<String>,
        /// The answer.
        pub(in crate::tasks) outcome: Outcome,
    }

    /// The schema exchange of Podium's discovery: it stores each offered schema
    /// that the same request declares and whose recomputed fingerprint matches,
    /// and wants every declared fingerprint it does not hold, sorted.
    #[derive(Default)]
    pub(in crate::tasks) struct FakeControlPlane {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        catalog: BTreeMap<String, Vec<u8>>,
        seen: Vec<Seen>,
        unavailable: BTreeSet<usize>,
        never_stored: BTreeSet<String>,
        /// A cap below Podium's, as a proxy in front of it may set.
        max_bytes: Option<usize>,
    }

    impl FakeControlPlane {
        /// A control plane behind a proxy that refuses requests over
        /// `max_bytes`, with the answers Podium gives over its own cap.
        pub(in crate::tasks) fn limited(max_bytes: usize) -> Self {
            let control = Self::default();
            control.lock().max_bytes = Some(max_bytes);
            control
        }

        /// A control plane that fails the requests numbered in `requests`,
        /// counting from 1.
        pub(in crate::tasks) fn failing(requests: impl IntoIterator<Item = usize>) -> Self {
            let control = Self::default();
            control.lock().unavailable.extend(requests);
            control
        }

        /// A control plane that never manages to store `fingerprint`, as after a
        /// store error.
        pub(in crate::tasks) fn losing(fingerprint: &str) -> Self {
            let control = Self::default();
            control.lock().never_stored.insert(fingerprint.to_owned());
            control
        }

        /// Answers one request of `bytes` encoded bytes.
        pub(in crate::tasks) fn answer(&self, request: &SyncRequest, bytes: usize) -> Outcome {
            let mut state = self.lock();
            let number = state.seen.len() + 1;
            let outcome = if bytes > state.max_bytes.unwrap_or(PODIUM_MAX_SYNC_BYTES) {
                Outcome::TooLarge
            } else if state.unavailable.contains(&number) {
                Outcome::Unavailable
            } else {
                Outcome::Accepted(state.exchange(request))
            };
            state.seen.push(Seen {
                bytes,
                schemas: request
                    .schemas
                    .iter()
                    .map(|schema| schema.fingerprint.clone())
                    .collect(),
                outcome: outcome.clone(),
            });
            outcome
        }

        /// Every request received so far, in order.
        pub(in crate::tasks) fn seen(&self) -> Vec<Seen> {
            self.lock().seen.clone()
        }

        /// The stored schemas, by fingerprint.
        pub(in crate::tasks) fn catalog(&self) -> BTreeMap<String, Vec<u8>> {
            self.lock().catalog.clone()
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().expect("fake control plane lock")
        }
    }

    impl State {
        fn exchange(&mut self, request: &SyncRequest) -> Vec<String> {
            let declared: BTreeSet<String> = request
                .capabilities
                .iter()
                .flat_map(|capabilities| &capabilities.runners)
                .flat_map(|runner| &runner.workloads)
                .map(|workload| workload.schema_fingerprint.clone())
                .filter(|fingerprint| !fingerprint.is_empty())
                .collect();
            for offered in &request.schemas {
                let stored = declared.contains(&offered.fingerprint)
                    && !self.never_stored.contains(&offered.fingerprint)
                    && fingerprint_of(&offered.schema).as_ref() == Some(&offered.fingerprint);
                if stored {
                    self.catalog
                        .entry(offered.fingerprint.clone())
                        .or_insert_with(|| offered.schema.clone());
                }
            }
            declared
                .into_iter()
                .filter(|fingerprint| !self.catalog.contains_key(fingerprint))
                .collect()
        }
    }

    /// Podium's fingerprint of offered bytes: the SHA-256 of their RFC 8785
    /// canonical JSON.
    fn fingerprint_of(schema: &[u8]) -> Option<String> {
        let value: serde_json::Value = serde_json::from_slice(schema).ok()?;
        let canonical = solti_model::canonical_json(&value).ok()?;
        Some(
            Sha256::digest(&canonical)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    /// A fake serving one transport on a loopback port.
    pub(in crate::tasks) struct FakeServer {
        address: SocketAddr,
        shutdown: oneshot::Sender<()>,
        task: JoinHandle<()>,
    }

    impl FakeServer {
        pub(in crate::tasks) fn new(
            address: SocketAddr,
            shutdown: oneshot::Sender<()>,
            task: JoinHandle<()>,
        ) -> Self {
            Self {
                address,
                shutdown,
                task,
            }
        }

        /// The control-plane endpoint of the fake.
        pub(in crate::tasks) fn endpoint(&self) -> String {
            format!("http://{}", self.address)
        }

        /// Stops the fake and waits until it has.
        pub(in crate::tasks) async fn stop(self) {
            let _ = self.shutdown.send(());
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("the fake control plane stops within the bound")
                .expect("the fake control plane does not panic");
        }
    }

    /// Serves `control` over `transport` on an ephemeral loopback port.
    pub(in crate::tasks) async fn serve(
        transport: DiscoveryTransport,
        control: Arc<FakeControlPlane>,
    ) -> FakeServer {
        match transport {
            #[cfg(feature = "grpc")]
            DiscoveryTransport::Grpc => super::grpc::fake::serve(control).await,
            #[cfg(feature = "http")]
            DiscoveryTransport::Http => super::http::fake::serve(control).await,
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(all(feature = "grpc", feature = "http"))]
    use solti_model::AgentId;

    use super::*;

    #[cfg(all(feature = "grpc", feature = "http"))]
    fn config(transport: DiscoveryTransport) -> DiscoverConfig {
        DiscoverConfig::builder(
            AgentId::new("agent-1").unwrap(),
            "agent-1",
            crate::AgentEndpoint::new("http://127.0.0.1:8085", crate::AgentEndpointType::Http, 1)
                .unwrap(),
            crate::ControlPlaneEndpoint::new("http://127.0.0.1:9000", transport).unwrap(),
            solti_model::AgentCapabilities::default(),
            30_000,
            "test@1",
        )
        .build()
        .expect("config builds")
    }

    #[cfg(all(feature = "grpc", feature = "http"))]
    #[test]
    fn instantiates_only_the_selected_transport_adapter() {
        let http = TransportAdapter::from_config(&config(DiscoveryTransport::Http))
            .expect("http adapter builds");
        assert!(matches!(http, TransportAdapter::Http(_)));

        let grpc = TransportAdapter::from_config(&config(DiscoveryTransport::Grpc))
            .expect("grpc adapter builds");
        assert!(matches!(grpc, TransportAdapter::Grpc(_)));
    }

    #[test]
    fn response_validation_preserves_the_wire_contract() {
        let accepted = SyncResponse {
            success: true,
            reason: String::new(),
            retry_after_s: 0,
            want_schemas: vec!["a".repeat(64)],
        };
        assert_eq!(
            validate_response(accepted.clone()).expect("accepted"),
            accepted,
            "an accepted response comes back whole, want_schemas included"
        );

        for (reason, retry_after_s, expected_reason, expected_retry_after_s) in [
            ("", 0, "control plane returned success=false", None),
            ("overloaded", 60, "overloaded", Some(60)),
            ("bad", -5, "bad", None),
        ] {
            let result = validate_response(SyncResponse {
                success: false,
                reason: reason.into(),
                retry_after_s,
                want_schemas: Vec::new(),
            });
            match result {
                Err(DiscoverError::Rejected {
                    reason,
                    retry_after_s,
                }) => {
                    assert_eq!(reason, expected_reason);
                    assert_eq!(retry_after_s, expected_retry_after_s);
                }
                other => panic!("expected Rejected, got {other:?}"),
            }
        }
    }
}
