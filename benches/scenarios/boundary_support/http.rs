//! Controlled public extension runner and real loopback HTTP fixture.

use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use reqwest::{Client, Response};
use serde_json::{Value, json};
use solti_api::{HttpApi, SupervisorApiAdapter};
use solti_benches::fixtures::bounded;
use solti_core::{OutputConfig, SupervisorApi};
use solti_model::{
    AdmissionPolicy, ExtensionWorkload, Labels, RestartPolicy, Task, TaskId, TaskManifest,
    TaskPhase, TaskSpec, TaskWorkload, Token, WorkloadTypeMeta,
};
use solti_runner::{BuildContext, RunId, Runner, RunnerError, RunnerRouter};
use taskvisor::{TaskContext, TaskError, TaskFn, TaskRef};
use tokio::sync::{Semaphore, oneshot};

pub const TOKEN: &str = "loopback-benchmark-only";
const API_VERSION: &str = "benches.solti.io/v1";
const KIND: &str = "ControlledJob";

#[derive(Clone, Default)]
pub struct RunnerOptions {
    pub gate: Option<Arc<Semaphore>>,
    pub chunks: usize,
    pub chunk_bytes: usize,
}

struct ControlledRunner(RunnerOptions);

#[solti_runner::async_trait]
impl Runner for ControlledRunner {
    fn name(&self) -> &str {
        "benchmark-controlled"
    }

    fn workload_types(&self) -> Vec<WorkloadTypeMeta> {
        vec![WorkloadTypeMeta::new(API_VERSION, KIND).unwrap()]
    }

    async fn build_task(
        &self,
        task: &Task,
        _: &RunId,
        context: &BuildContext,
        _: &solti_runner::BuildCancellation,
        _: &mut solti_runner::BuildScope,
    ) -> Result<TaskRef, RunnerError> {
        let options = self.0.clone();
        let name = task.name().clone();
        let generation = task.metadata().generation();
        let output = Arc::clone(context.output_publisher());
        let payload = Bytes::from(vec![b'x'; options.chunk_bytes]);
        Ok(TaskFn::arc(move |context: TaskContext| {
            let options = options.clone();
            let name = name.clone();
            let output = Arc::clone(&output);
            let payload = payload.clone();
            async move {
                if let Some(gate) = options.gate {
                    context
                        .run_until_cancelled(gate.acquire())
                        .await?
                        .map_err(|_| TaskError::fatal("fixture output gate closed"))?
                        .forget();
                }
                if options.chunks > 0 {
                    // Every fixture manifest is RestartPolicy::Never: this is attempt one.
                    let sink = output
                        .sink_for(&name, generation, 1)
                        .ok_or_else(|| TaskError::fatal("fixture output is unavailable"))?;
                    for index in 0..options.chunks {
                        if index % 2 == 0 {
                            sink.stdout_line(payload.clone());
                        } else {
                            sink.stderr_line(payload.clone());
                        }
                    }
                }
                Ok(())
            }
        }))
    }
}

/// Queued success fixture: HTTP deletion is not a controller-Idle barrier.
pub fn manifest(name: &str, payload_bytes: usize) -> TaskManifest {
    let workload = TaskWorkload::Extension(
        ExtensionWorkload::new(
            API_VERSION,
            KIND,
            json!({"payload": "x".repeat(payload_bytes)}),
        )
        .unwrap(),
    );
    let spec = TaskSpec::builder(name, workload, 30_000_u64)
        .admission(AdmissionPolicy::Queue)
        .restart(RestartPolicy::Never)
        .build()
        .unwrap();
    let mut labels = Labels::new();
    labels.insert("dataset", "benchmark");
    TaskManifest::new(name, spec)
        .unwrap()
        .with_labels(labels)
        .unwrap()
}

pub struct HttpFixture {
    pub supervisor: Arc<SupervisorApi>,
    pub client: Client,
    pub tasks_url: String,
    stop: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl HttpFixture {
    pub async fn new(options: RunnerOptions, auth: bool) -> Self {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let mut router = RunnerRouter::new();
        router
            .register(Arc::new(ControlledRunner(options)))
            .unwrap();
        let supervisor = Arc::new(
            bounded(
                SupervisorApi::builder(router)
                    .with_output_config(OutputConfig::try_new(1_024).unwrap())
                    .start(),
            )
            .await
            .unwrap(),
        );
        let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
        let mut api = HttpApi::new(handler);
        let mut headers = reqwest::header::HeaderMap::new();
        if auth {
            api = api.with_auth(Token::new(TOKEN).unwrap());
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {TOKEN}").parse().unwrap(),
            );
        }
        let app = api.router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopping) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopping.await;
                })
                .await
                .unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let fixture = Self {
            supervisor,
            client,
            tasks_url: format!("http://{address}/apis/solti.io/v1/tasks"),
            stop,
            server,
        };
        // Warm the HTTP connection without retaining a Task.
        let response = bounded(fixture.client.get(&fixture.tasks_url).send())
            .await
            .unwrap();
        assert!(response.status().is_success());
        bounded(response.bytes()).await.unwrap();
        fixture
    }

    pub async fn create(&self, manifest: &TaskManifest) -> Task {
        let response = bounded(self.client.post(&self.tasks_url).json(manifest).send())
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
        bounded(response.json()).await.unwrap()
    }

    pub async fn observed_success(&self, name: &TaskId) -> Task {
        bounded(async {
            loop {
                let response = self
                    .client
                    .get(format!("{}/{name}", self.tasks_url))
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
                let task: Task = response.json().await.unwrap();
                if task.phase() == &TaskPhase::Succeeded {
                    return task;
                }
                assert!(
                    !task.phase().is_terminal(),
                    "unexpected terminal task: {:?}",
                    task.phase()
                );
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    pub async fn delete(&self, name: &TaskId) {
        let response = bounded(
            self.client
                .delete(format!("{}/{name}", self.tasks_url))
                .send(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
        bounded(response.bytes()).await.unwrap();
    }

    pub async fn close(self) {
        bounded(self.supervisor.shutdown()).await.unwrap();
        drop(self.client);
        self.stop.send(()).unwrap();
        bounded(self.server).await.unwrap();
    }
}

/// Incremental framing; retained data is capped even if a server stops producing delimiters.
pub struct FramedBody {
    response: Response,
    pending: BytesMut,
}

impl FramedBody {
    pub fn new(response: Response, content_type: &str) -> Self {
        assert!(response.status().is_success());
        assert!(
            response.headers()[reqwest::header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(content_type)
        );
        Self {
            response,
            pending: BytesMut::new(),
        }
    }

    async fn frame(&mut self, delimiter: &[u8]) -> Bytes {
        bounded(async {
            loop {
                if let Some(position) = self
                    .pending
                    .windows(delimiter.len())
                    .position(|bytes| bytes == delimiter)
                {
                    let frame = self.pending.split_to(position).freeze();
                    self.pending.advance(delimiter.len());
                    return frame;
                }
                let chunk = self
                    .response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("stream ended before expected records");
                self.pending.extend_from_slice(&chunk);
                assert!(
                    self.pending.len() <= 8 * 1024 * 1024,
                    "unbounded fixture stream frame"
                );
            }
        })
        .await
    }

    pub async fn ndjson(&mut self) -> Value {
        serde_json::from_slice(&self.frame(b"\n").await).unwrap()
    }

    pub async fn sse(&mut self) -> Value {
        loop {
            let frame = self.frame(b"\n\n").await;
            let text = std::str::from_utf8(&frame).unwrap();
            if let Some(data) = text.lines().find_map(|line| {
                line.strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
            }) {
                return serde_json::from_str(data).unwrap();
            }
            // SSE keep-alive comments do not count as output events.
        }
    }
}
