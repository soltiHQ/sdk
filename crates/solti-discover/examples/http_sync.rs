//! # HTTP discovery sync
//!
//! One embedded discovery task sends agent identity and liveness to a control plane.
//! The advertised agent endpoint is independent of the outbound discovery endpoint.
//!
//! This example shows:
//!
//! - a complete `DiscoverConfig`;
//! - metadata and runner capability advertisement, with schema fingerprints;
//! - bearer authentication;
//! - the generated embedded task manifest;
//! - one real HTTP/JSON discovery v1 attempt: a heartbeat, then a follow-up
//!   that carries the schema the control plane asked for;
//! - protobuf JSON field encoding;
//! - custom uptime and metrics ports.
//!
//! The local server exists only to keep the example self-contained.
//! `solti-discover` itself is a client and does not run a server.
//! The explicit insecure-token opt-in is limited to this loopback example.
//! Use HTTPS whenever a real bearer token is configured.
//!
//! Run with `cargo run -p solti-discover --example http_sync --features http`.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use solti_discover::{
    AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DiscoverConfig, DiscoverFailReason,
    DiscoverMetricsBackend, DiscoverMetricsHandle, DiscoveryTransport, sync,
};
use solti_model::{
    AdmissionPolicy, AgentCapabilities, AgentId, Labels, RestartPolicy, RunnerCapability,
    TaskWorkload, Token, WorkloadSchema, WorkloadType, WorkloadTypeMeta,
};
use taskvisor::TaskContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-discover: one HTTP discovery attempt with the schema exchange

  API served by the agent
  AgentEndpoint(http://127.0.0.1:8085, HTTP, API v1)
                    │ advertised inside SyncRequest
  identity + metadata + capabilities (GVK + schema fingerprint) + uptime
                    ▼
             DiscoverConfig
                    │ sync(config, uptime)
                    ▼
       TaskManifest + embedded TaskRef
                              │ spawn one attempt
                              ▼
  local control plane ◄── POST /control/api/v1/discovery/sync   heartbeat, no schema
      │                       Authorization: Bearer ...
      │                       protobuf JSON body
      └── { "success": true, "wantSchemas": [fingerprint] }
                              │ follow-up in the same attempt
                              ▼
  local control plane ◄── POST ... "schemas": [{fingerprint, schema (base64)}]
      └── { "success": true } ──► success metrics ──► Ok

  AgentEndpoint is advertised for inbound Task API calls.
  ControlPlaneEndpoint is used for outbound discovery sync.
"#;

#[derive(Debug, Default)]
struct RecordingMetrics {
    attempts: AtomicUsize,
    successes: AtomicUsize,
    failures: AtomicUsize,
    success_durations_ms: Mutex<Vec<u64>>,
}

impl DiscoverMetricsBackend for RecordingMetrics {
    fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    fn record_success(&self, duration_ms: u64) {
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.success_durations_ms
            .lock()
            .expect("metrics recorder lock must not be poisoned")
            .push(duration_ms);
    }

    fn record_failure(&self, _duration_ms: u64, _reason: DiscoverFailReason) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }
}

struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_headers(header: &str) -> io::Result<(String, String, BTreeMap<String, String>)> {
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| io::Error::other("HTTP request has no request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| io::Error::other("HTTP request has no method"))?
        .to_owned();
    let path = request_parts
        .next()
        .ok_or_else(|| io::Error::other("HTTP request has no path"))?
        .to_owned();

    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid HTTP header"))?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    Ok((method, path, headers))
}

/// Reads one request and answers it with `response_body`, then closes the connection.
async fn capture_request(
    mut socket: TcpStream,
    response_body: &[u8],
) -> io::Result<CapturedRequest> {
    // Podium's cap on one discovery request; the SDK packs schemas below it.
    const MAX_REQUEST_BYTES: usize = 256 * 1024;

    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    let (header_end, content_length) = loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::other("HTTP client closed an incomplete request"));
        }
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_REQUEST_BYTES + 4_096 {
            return Err(io::Error::other(
                "HTTP request exceeded the control-plane limit",
            ));
        }
        let Some(header_end) = find_header_end(&request) else {
            continue;
        };
        let header = std::str::from_utf8(&request[..header_end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let (_, _, headers) = parse_headers(header)?;
        let content_length = headers
            .get("content-length")
            .ok_or_else(|| io::Error::other("HTTP request has no Content-Length"))?
            .parse::<usize>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if content_length > MAX_REQUEST_BYTES {
            return Err(io::Error::other(
                "HTTP request exceeded the control-plane limit",
            ));
        }
        if request.len() >= header_end + 4 + content_length {
            break (header_end, content_length);
        }
    };

    let header = std::str::from_utf8(&request[..header_end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (method, path, headers) = parse_headers(header)?;
    let body_start = header_end + 4;
    let body = serde_json::from_slice(&request[body_start..body_start + content_length])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response_body.len(),
    );
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(response_body).await?;
    socket.shutdown().await?;

    Ok(CapturedRequest {
        method,
        path,
        headers,
        body,
    })
}

/// The `ImageResize` body schema: the agent advertises its fingerprint and
/// sends these canonical bytes only when the control plane asks.
fn image_resize_schema() -> Result<WorkloadSchema, solti_model::WorkloadSchemaError> {
    WorkloadSchema::from_value(&serde_json::json!({
        "type": "object",
        "properties": {
            "width": {"type": "integer", "minimum": 1},
            "height": {"type": "integer", "minimum": 1}
        },
        "required": ["width", "height"]
    }))
}

fn capabilities(resize: &WorkloadSchema) -> Result<AgentCapabilities, solti_model::ModelError> {
    let mut labels = Labels::new();
    labels.insert("solti.io/runner-name", "local");
    labels.insert("topology.solti.io/zone", "lab-1");
    let backup = WorkloadSchema::from_value(&serde_json::json!({
        "type": "object",
        "properties": {"database": {"type": "string"}},
        "required": ["database"]
    }))?;
    AgentCapabilities::new(vec![RunnerCapability::new(
        "local",
        labels,
        vec![
            WorkloadType::new(
                WorkloadTypeMeta::new("jobs.example.io/v1", "ImageResize")?,
                resize.clone(),
            ),
            WorkloadType::new(
                WorkloadTypeMeta::new("jobs.example.io/v1", "DatabaseBackup")?,
                backup,
            ),
        ],
    )?])
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Observe the exact HTTP requests produced by one embedded discovery attempt."
    );

    let resize = image_resize_schema()?;
    let fingerprint = resize.fingerprint().as_str().to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let control_address = listener.local_addr()?;
    let want = format!(r#"{{"success":true,"wantSchemas":["{fingerprint}"]}}"#);
    let server = tokio::spawn(async move {
        let (socket, peer) = listener.accept().await?;
        println!("[control-plane] Accepted the heartbeat from {peer}; asking for one schema.");
        let heartbeat = capture_request(socket, want.as_bytes()).await?;
        let (socket, peer) = listener.accept().await?;
        println!("[control-plane] Accepted the follow-up from {peer}.");
        let follow_up = capture_request(socket, br#"{"success":true}"#).await?;
        Ok::<_, io::Error>((heartbeat, follow_up))
    });

    let metrics = Arc::new(RecordingMetrics::default());
    let metrics_handle: DiscoverMetricsHandle = metrics.clone();
    let mut metadata = HashMap::new();
    metadata.insert("environment".into(), "development".into());
    metadata.insert("region".into(), "local".into());

    let config = DiscoverConfig::builder(
        AgentId::new("agent-http-1")?,
        " Agent HTTP 1 ",
        AgentEndpoint::new("http://127.0.0.1:8085", AgentEndpointType::Http, 1)?,
        ControlPlaneEndpoint::new(
            format!("http://{control_address}/control"),
            DiscoveryTransport::Http,
        )?,
        capabilities(&resize)?,
        1,
        "http-discovery@1",
    )
    .metadata(metadata)
    .with_token(Token::new("example-token")?)
    .allow_insecure_token_transport()
    .with_metrics(metrics_handle)
    .build()?;
    println!("[config] Advertised HTTP Task API v1 at http://127.0.0.1:8085.");
    println!("[config] Selected outbound HTTP discovery at http://{control_address}/control.");
    println!("[config] Explicitly allowed the teaching token on loopback.");
    println!("[config] A real bearer token requires HTTPS.");

    let uptime = Arc::new(|| 42_u64);
    let (manifest, task_ref) = sync(config, uptime)?;
    let TaskWorkload::Embedded(embedded) = manifest.spec().workload() else {
        return Err(io::Error::other("discovery manifest is not Embedded").into());
    };
    println!(
        "[task] name={}, slot={}, revision={}, timeout={}ms.",
        manifest.name(),
        manifest.spec().slot(),
        embedded.revision(),
        manifest.spec().timeout().as_millis(),
    );
    println!(
        "[task] restart={:?}, admission={:?}.",
        manifest.spec().restart(),
        manifest.spec().admission(),
    );
    assert_eq!(
        manifest.spec().restart(),
        RestartPolicy::Always {
            interval_ms: Some(1)
        }
    );
    assert_eq!(manifest.spec().admission(), AdmissionPolicy::Replace);

    task_ref.spawn(TaskContext::detached()).await?;
    println!("[attempt] Control plane accepted the heartbeat and the follow-up.");
    let (heartbeat, follow_up) = server.await??;

    println!("[wire] {} {}", heartbeat.method, heartbeat.path);
    println!(
        "[wire] authorization={:?}, content-type={:?}.",
        heartbeat.headers.get("authorization"),
        heartbeat.headers.get("content-type"),
    );
    println!("[wire] heartbeat JSON body:");
    println!("{}", serde_json::to_string_pretty(&heartbeat.body)?);
    assert_eq!(heartbeat.method, "POST");
    assert_eq!(heartbeat.path, "/control/api/v1/discovery/sync");
    assert_eq!(
        heartbeat.headers.get("authorization").map(String::as_str),
        Some("Bearer example-token"),
    );
    assert_eq!(heartbeat.body["id"], "agent-http-1");
    assert_eq!(heartbeat.body["name"], "Agent HTTP 1");
    assert_eq!(heartbeat.body["endpoint"], "http://127.0.0.1:8085");
    assert_eq!(heartbeat.body["endpointType"], "ENDPOINT_TYPE_HTTP");
    assert_eq!(heartbeat.body["apiVersion"], 1);
    assert_eq!(heartbeat.body["heartbeatIntervalS"], 1);
    assert_eq!(heartbeat.body["uptimeSeconds"], "42");
    assert!(heartbeat.body["ts"].as_str().is_some());
    assert_eq!(heartbeat.body["metadata"]["region"], "local");
    let runner = &heartbeat.body["capabilities"]["runners"][0];
    assert_eq!(runner["name"], "local");
    assert!(
        runner["workloads"]
            .as_array()
            .is_some_and(|workloads| workloads.iter().any(|workload| {
                workload["kind"] == "ImageResize"
                    && workload["schemaFingerprint"] == fingerprint.as_str()
            })),
        "every declared type carries its schema fingerprint"
    );
    assert!(
        heartbeat.body.get("schemas").is_none(),
        "the first heartbeat carries no schema"
    );

    let sent = &follow_up.body["schemas"][0];
    println!(
        "[wire] follow-up schemas[0].fingerprint={}, schema={} base64 characters.",
        sent["fingerprint"],
        sent["schema"].as_str().map_or(0, str::len),
    );
    assert_eq!(
        follow_up.body["schemas"].as_array().map(Vec::len),
        Some(1),
        "only the requested schema is sent"
    );
    assert_eq!(sent["fingerprint"], fingerprint.as_str());
    let canonical_base64 = sent["schema"]
        .as_str()
        .ok_or_else(|| io::Error::other("schema bytes are not a base64 string"))?;
    assert_eq!(
        canonical_base64.len(),
        resize.canonical_json().len().div_ceil(3) * 4,
        "the canonical bytes travel as standard base64"
    );

    let durations = metrics
        .success_durations_ms
        .lock()
        .expect("metrics recorder lock must not be poisoned");
    println!(
        "[metrics] attempts={}, successes={}, failures={}, durationMs={:?}.",
        metrics.attempts.load(Ordering::Relaxed),
        metrics.successes.load(Ordering::Relaxed),
        metrics.failures.load(Ordering::Relaxed),
        &*durations,
    );
    assert_eq!(metrics.attempts.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.successes.load(Ordering::Relaxed), 2);
    assert_eq!(metrics.failures.load(Ordering::Relaxed), 0);

    println!(
        "\nResult: one embedded attempt advertised identity, Task API coordinates, capabilities with schema fingerprints, metadata, timestamp, and application-owned uptime, then sent the one schema the control plane requested in a follow-up over discovery HTTP v1."
    );
    Ok(())
}
