//! # HTTP task contract
//!
//! `HttpApi` converts Kubernetes-shaped JSON into domain values for one `ApiHandler`.
//! The returned axum router can be mounted or exercised without opening a socket.
//!
//! This example shows:
//!
//! - generated OpenAPI 3.1 metadata;
//! - bearer authentication before handler execution;
//! - one CRD-shaped create request and response;
//! - an application-owned extension workload;
//! - live output encoded as Server-Sent Events;
//! - a small custom handler behind the transport boundary.
//!
//! Run with `cargo run -p solti-api --example http_contract --features http`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use solti_api::axum::body::Body;
use solti_api::axum::http::{Method, Request, StatusCode};
use solti_api::{
    ApiError, ApiHandler, HTTP_API_ROOT, HttpApi, OutputEventStream, TaskWatchEventStream,
};
use solti_model::{
    ExtensionWorkload, OutputChunk, OutputEvent, StreamKind, Task, TaskFilter, TaskId,
    TaskManifest, TaskPage, TaskQuery, TaskRunPage, TaskRunQuery, TaskSpec, TaskWorkload, Token,
    WritePreconditions,
};
use tower::ServiceExt;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-api: HTTP/JSON transport

  CRD JSON request
        │ Authorization: Bearer ...
        ▼
  axum Router under /apis/solti.io/v1
        ├──► authentication + body limit + validation
        │
        └──► ApiHandler ──► application backend
                    │
                    ├──► Task JSON response
                    └──► OutputEvent stream ──► Server-Sent Events

  HttpApi::build also returns the OpenAPI document for the mounted routes.
  The handler receives validated solti-model values, not raw HTTP data.
"#;

#[derive(Default)]
struct MemoryHandler {
    tasks: Mutex<BTreeMap<String, Task>>,
    resource_version: AtomicU64,
}

impl MemoryHandler {
    fn task_from_manifest(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        let mut task = Task::from_manifest(manifest)
            .map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
        let version = self.resource_version.fetch_add(1, Ordering::SeqCst) + 1;
        task.set_resource_version(format!("memory:{version}"))
            .map_err(|error| ApiError::Internal(error.to_string()))?;
        Ok(task)
    }

    fn lock_tasks(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Task>>, ApiError> {
        self.tasks
            .lock()
            .map_err(|_| ApiError::Internal("example task store lock is poisoned".into()))
    }
}

#[async_trait]
impl ApiHandler for MemoryHandler {
    async fn create_task(&self, manifest: TaskManifest) -> Result<Task, ApiError> {
        let name = manifest.name().to_string();
        let mut tasks = self.lock_tasks()?;
        if tasks.contains_key(&name) {
            return Err(ApiError::AlreadyExists(name));
        }
        let task = self.task_from_manifest(manifest)?;
        tasks.insert(name, task.clone());
        Ok(task)
    }

    async fn apply_task(
        &self,
        _manifest: TaskManifest,
        _preconditions: WritePreconditions,
    ) -> Result<Task, ApiError> {
        Err(ApiError::MethodNotAllowed(
            "the teaching backend implements create and read operations only".into(),
        ))
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>, ApiError> {
        Ok(self.lock_tasks()?.get(id.as_str()).cloned())
    }

    async fn query_tasks(&self, query: TaskQuery) -> Result<TaskPage<Task>, ApiError> {
        if query.continuation().is_some() {
            return Err(ApiError::MethodNotAllowed(
                "the teaching backend does not retain list snapshots".into(),
            ));
        }
        let items = self
            .lock_tasks()?
            .values()
            .filter(|task| query.matches(task))
            .take(query.limit())
            .cloned()
            .collect();
        Ok(TaskPage {
            items,
            resource_version: format!("memory:{}", self.resource_version.load(Ordering::SeqCst)),
            continuation: None,
            remaining_item_count: 0,
        })
    }

    async fn watch_tasks(
        &self,
        _filter: TaskFilter,
        _resource_version: Option<String>,
    ) -> Result<TaskWatchEventStream, ApiError> {
        Ok(Box::pin(tokio_stream::empty()))
    }

    async fn query_task_runs(
        &self,
        id: &TaskId,
        query: TaskRunQuery,
    ) -> Result<TaskRunPage, ApiError> {
        if query.continuation().is_some() {
            return Err(ApiError::MethodNotAllowed(
                "the teaching backend does not retain run snapshots".into(),
            ));
        }
        let task = self
            .lock_tasks()?
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| ApiError::TaskNotFound(id.to_string()))?;
        Ok(TaskRunPage {
            items: Vec::new(),
            task: id.clone(),
            task_uid: task.metadata().uid().clone(),
            resource_version: "runs-memory:1".into(),
            continuation: None,
            remaining_item_count: 0,
        })
    }

    async fn cancel_task(
        &self,
        _id: &TaskId,
        _preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        Err(ApiError::MethodNotAllowed(
            "the teaching backend does not execute tasks".into(),
        ))
    }

    async fn delete_task(
        &self,
        id: &TaskId,
        preconditions: WritePreconditions,
    ) -> Result<(), ApiError> {
        if !preconditions.is_empty() {
            return Err(ApiError::MethodNotAllowed(
                "the teaching backend does not implement conditional delete".into(),
            ));
        }
        self.lock_tasks()?
            .remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| ApiError::TaskNotFound(id.to_string()))
    }

    async fn stream_task_logs(
        &self,
        id: &TaskId,
        task_uid: &solti_model::Uid,
    ) -> Result<OutputEventStream, ApiError> {
        if self
            .lock_tasks()?
            .get(id.as_str())
            .is_none_or(|task| task.uid() != task_uid)
        {
            return Err(ApiError::TaskNotFound(id.to_string()));
        }
        let events = vec![
            OutputEvent::RunStarted {
                generation: 1,
                attempt: 1,
                started_at: UNIX_EPOCH + Duration::from_millis(1_000),
            },
            OutputEvent::Chunk(OutputChunk {
                generation: 1,
                attempt: 1,
                stream: StreamKind::Stdout,
                seq: 0,
                ts: UNIX_EPOCH + Duration::from_millis(1_100),
                line: Bytes::from_static(b"resized cover.png"),
                truncated: false,
            }),
            OutputEvent::RunFinished {
                generation: 1,
                attempt: 1,
                exit_code: Some(0),
                finished_at: UNIX_EPOCH + Duration::from_millis(1_200),
            },
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }
}

fn manifest() -> ExampleResult<TaskManifest> {
    let workload = TaskWorkload::Extension(ExtensionWorkload::new(
        "media.example.io/v1",
        "ImageResize",
        json!({
            "source": "cover.png",
            "width": 1280
        }),
    )?);
    let spec = TaskSpec::builder("image-processing", workload, 30_000_u64).build()?;
    Ok(TaskManifest::new("resize-cover", spec)?)
}

fn request(
    method: Method,
    uri: &str,
    body: Vec<u8>,
    token: Option<&str>,
) -> ExampleResult<Request<Body>> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    Ok(builder.body(Body::from(body))?)
}

async fn response_json(response: solti_api::axum::response::Response) -> ExampleResult<Value> {
    let bytes = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Send real HTTP requests through the generated router and inspect JSON, authentication, OpenAPI, and SSE output."
    );

    let handler = Arc::new(MemoryHandler::default());
    let token = Token::new("example-token")?;
    let parts = HttpApi::new(Arc::clone(&handler)).with_auth(token).build();
    let openapi = serde_json::to_value(&parts.openapi)?;
    let path_count = openapi["paths"].as_object().map_or(0, serde_json::Map::len);
    println!(
        "[openapi] version={}, paths={}, createOperation={}.",
        openapi["openapi"],
        path_count,
        openapi["paths"][format!("{HTTP_API_ROOT}/tasks")]["post"]["operationId"],
    );

    let create_uri = format!("{HTTP_API_ROOT}/tasks");
    let payload = serde_json::to_vec(&manifest()?)?;
    println!("[wire] POST {create_uri}");
    println!("[wire] Request JSON:");
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::from_slice::<Value>(&payload)?)?
    );

    let unauthorized = parts
        .router
        .clone()
        .oneshot(request(Method::POST, &create_uri, payload.clone(), None)?)
        .await?;
    let unauthorized_status = unauthorized.status();
    let unauthorized_body = response_json(unauthorized).await?;
    println!(
        "[auth] Missing bearer token: status={}, reason={}.",
        unauthorized_status, unauthorized_body["reason"],
    );
    assert_eq!(unauthorized_status, StatusCode::UNAUTHORIZED);

    let created = parts
        .router
        .clone()
        .oneshot(request(
            Method::POST,
            &create_uri,
            payload,
            Some("example-token"),
        )?)
        .await?;
    let created_status = created.status();
    let created_body = response_json(created).await?;
    println!(
        "[create] status={}, name={}, resourceVersion={}, workload={}/{}.",
        created_status,
        created_body["metadata"]["name"],
        created_body["metadata"]["resourceVersion"],
        created_body["spec"]["workload"]["apiVersion"],
        created_body["spec"]["workload"]["kind"],
    );
    assert_eq!(created_status, StatusCode::CREATED);
    assert_eq!(created_body["metadata"]["name"], "resize-cover");

    let task_uid = created_body["metadata"]["uid"]
        .as_str()
        .ok_or("created Task has no UID")?;
    let logs_uri = format!("{HTTP_API_ROOT}/tasks/resize-cover/logs?taskUid={task_uid}");
    let logs = parts
        .router
        .oneshot(request(
            Method::GET,
            &logs_uri,
            Vec::new(),
            Some("example-token"),
        )?)
        .await?;
    assert_eq!(logs.status(), StatusCode::OK);
    let content_type = logs
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("missing")
        .to_owned();
    let sse = String::from_utf8(logs.into_body().collect().await?.to_bytes().to_vec())?;
    println!("[logs] content-type={content_type}.");
    println!("[logs] SSE body:\n{sse}");
    assert!(sse.contains("event: run-started"));
    assert!(sse.contains("event: chunk"));
    assert!(sse.contains("event: run-finished"));

    println!(
        "Result: one handler powered authenticated CRD JSON writes, generated OpenAPI, and an SSE live-output stream."
    );
    Ok(())
}
