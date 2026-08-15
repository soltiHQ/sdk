# solti-api

`solti-api` exposes the Solti task contract over HTTP/JSON and gRPC.
Both transports use one `ApiHandler`.
The optional `SupervisorApiAdapter` connects that handler to `solti-core`.

Use this crate when an agent binary needs a public task API.
The crate validates wire input, converts transport values, streams changes and output, and maps errors.
It does not store or execute tasks.

## Quick start

Build both transports from one running supervisor:

```rust,no_run
use std::sync::Arc;

use solti_api::{GrpcApi, HttpApi, HttpApiParts, SupervisorApiAdapter};

fn build_transports(supervisor: Arc<solti_core::SupervisorApi>) {
    let handler = Arc::new(SupervisorApiAdapter::new(supervisor));

    let HttpApiParts { router: http, openapi } =
        HttpApi::new(handler.clone()).build();
    let grpc = GrpcApi::new(handler).server();

    let _ = (http, openapi, grpc);
}
```

`HttpApi::build` returns the router and its OpenAPI document.
`HttpApi::router` is the convenience method when the document is not needed.
`GrpcApi::server` returns a service for `tonic::transport::Server`.
The application owns listener addresses, startup, and shutdown.

## What it does

- exposes create, apply, get, list, watch, run history, delete, and live output;
- keeps HTTP and gRPC on one handler contract;
- validates request fields before handler calls;
- maps domain errors to Kubernetes-style HTTP `Status` resources and gRPC status codes;
- supports bearer authentication on every route and RPC;
- records bounded transport metrics through `ApiMetricsBackend`;
- hides in-process `Embedded` workloads from public transports;
- enforces a 4 MiB request and message limit.

## Contracts

| Contract | Source                                             |
|----------|----------------------------------------------------|
| Behavior | [Task API contract](CONTRACT.md)                   |
| HTTP     | OpenAPI produced from the mounted `HttpApi` routes |
| gRPC     | [`solti.task.v1`](proto/solti/task/v1/api.proto)   |

OpenAPI defines HTTP routes and CRD JSON shapes.
Protobuf defines gRPC services and DTOs.
`CONTRACT.md` defines behavior shared by both transports.

The authoritative protobuf tree lives in
[`soltiHQ/proto`](https://github.com/soltiHQ/proto).
Release and CI tooling vendor a pinned revision into `proto/`.
The crate includes generated Rust bindings from that vendored contract.
Consumer builds do not run `protoc`.
The `generated_contract` test regenerates the binding and rejects drift.

`HttpApi::build` generates a standalone OpenAPI 3.1 document.
It uses JSON Schema draft 2020-12.
Configured bearer authentication is included on the task operations.

Use `HttpApi::mount` when the binary also owns routes:

```rust,no_run
use std::sync::Arc;

use solti_api::{
    ApiHandler, HttpApi,
    aide::{
        axum::{ApiRouter, routing::get_with},
        openapi::{Info, OpenApi},
    },
    axum::{Json, extract::State},
};

#[derive(Clone)]
struct AppState {
    ready: bool,
}

async fn health(State(state): State<AppState>) -> Json<bool> {
    Json(state.ready)
}

fn build<H: ApiHandler>(handler: Arc<H>) -> (solti_api::axum::Router, OpenApi) {
    let mut openapi = OpenApi {
        info: Info {
            title: "My Agent".into(),
            version: "1.0.0".into(),
            ..Info::default()
        },
        ..OpenApi::default()
    };

    let app = ApiRouter::<AppState>::new().api_route(
        "/health",
        get_with(health, |operation| operation.id("health")),
    );
    let app = HttpApi::new(handler).mount(app, &mut openapi);
    let router = app
        .with_state(AppState { ready: true })
        .finish_api(&mut openapi);

    (router, openapi)
}
```

The application calls `finish_api` once.
Its metadata, state, routes, and middleware remain application-owned.
Task authentication, limits, metrics, and fallbacks apply only below `HTTP_API_ROOT`.
Schema generation uses the Aide context selected by the application.

The Task API schemas exclude the SDK-only `Embedded` workload.
Other application schemas are not modified.
The in-process model keeps it.

## Inputs and outputs

| API or value             | Input                                      | Output                                      |
|--------------------------|--------------------------------------------|---------------------------------------------|
| `ApiHandler`             | Domain values from `solti-model`           | Domain results or `ApiError`                |
| `SupervisorApiAdapter`   | `Arc<solti_core::SupervisorApi>`           | Ready `ApiHandler` implementation           |
| `HttpApi::build`         | Handler, optional token and metrics        | `HttpApiParts` with router and OpenAPI      |
| `HttpApi::mount`         | Application `ApiRouter` and OpenAPI        | Router with the scoped Task API subtree     |
| `HttpApi::router`        | Handler, optional token and metrics        | `axum::Router` without retained OpenAPI     |
| `GrpcApi::server`        | Handler, optional token and metrics        | Tonic `GrpcServer` service                  |
| `grpc::wire`             | Generated protobuf request and response    | Current client and server types             |
| `to_tonic_server_tls`    | `solti_tls::ServerTlsConfig`               | Tonic server TLS config                     |
| `ApiMetricsBackend`      | Transport request lifecycle                | Application-defined metric updates          |

HTTP reads and writes the model-owned CRD JSON representation.
gRPC reads and writes versioned protobuf DTOs.
Both are converted to the same domain values before `ApiHandler` is called.

## Features

| Feature        | Default | Effect                                                |
|----------------|---------|-------------------------------------------------------|
| `core-adapter` | Off     | Enables `SupervisorApiAdapter`                        |
| `grpc`         | Off     | Enables the tonic server and `grpc::wire`             |
| `grpc-tls`     | Off     | Enables the `solti-tls` tonic adapter; implies `grpc` |
| `http`         | Off     | Enables the axum HTTP/JSON router                     |

No feature is enabled by default.
Enable only the transports and adapters used by the agent binary.

## Handler boundary

```text
HTTP request ── parse CRD JSON ──┐
                                 ▼
                           ApiHandler
                                 ├─ custom implementation
                                 └─ SupervisorApiAdapter ── solti-core
                                 ▲
gRPC call ── convert v1 DTO ─────┘
```

`ApiHandler` has eight operations:

| Operation          | Input                                             | Output                    |
|--------------------|---------------------------------------------------|---------------------------|
| `create_task`      | `TaskManifest`                                    | `Task`                    |
| `apply_task`       | `TaskManifest`, `WritePreconditions`              | `Task`                    |
| `get_task`         | `TaskId`                                          | Optional `Task`           |
| `query_tasks`      | `TaskQuery`                                       | `TaskPage<Task>`          |
| `watch_tasks`      | `TaskFilter`, optional resource version           | Task watch stream         |
| `query_task_runs`  | `TaskId`, `TaskRunQuery`                          | `TaskRunPage`             |
| `delete_task`      | `TaskId`, `WritePreconditions`                    | Empty success             |
| `stream_task_logs` | `TaskId`                                          | Live `OutputEvent` stream |

Implement the trait directly when the application has another backend.
Transport validation and public workload visibility still apply.

## HTTP

The current API root is `/apis/solti.io/v1`.

| Method   | Path                                          | Result                                  |
|----------|-----------------------------------------------|-----------------------------------------|
| `POST`   | `/apis/solti.io/v1/tasks`                     | Create a task; `201`                    |
| `PUT`    | `/apis/solti.io/v1/tasks/{name}`              | Apply a task; `200`                     |
| `GET`    | `/apis/solti.io/v1/tasks/{name}`              | Get a task                              |
| `GET`    | `/apis/solti.io/v1/tasks`                     | List tasks                              |
| `GET`    | `/apis/solti.io/v1/tasks?watch=true`          | Watch task resources                    |
| `GET`    | `/apis/solti.io/v1/tasks/{name}/runs`         | List run history                        |
| `GET`    | `/apis/solti.io/v1/tasks/{name}/logs`         | Stream live output with SSE             |
| `DELETE` | `/apis/solti.io/v1/tasks/{name}`              | Request logical cancellation and remove retained state; `204` |

List one filtered page:

```text
GET /apis/solti.io/v1/tasks?slot=jobs&phase=running&phase=pending&labelSelector=app%3Dbatch&limit=100
```

The response is a Kubernetes-shaped collection:

```json
{
  "apiVersion": "solti.io/v1",
  "kind": "TaskList",
  "metadata": {
    "resourceVersion": "opaque-version",
    "continue": "opaque-token",
    "remainingItemCount": 12
  },
  "items": []
}
```

`continue` and `remainingItemCount` are omitted on the final page.
`limit` is a count ceiling.
The bundled adapter first limits compact-JSON Task item payloads to 4 MiB.
It passes an oversized first Task through alone for native measurement.
Each HTTP TaskList body is also limited to 4 MiB of compact JSON.
The response contains the largest complete prefix from that domain page that
fits the HTTP limit.
One Task that cannot fit returns HTTP `429` with `reason=TooManyRequests`.

Run history uses the same count defaults and a separate stable snapshot:

```text
GET /apis/solti.io/v1/tasks/{name}/runs?limit=100&continue={opaque-token}
```

The response keeps the `runs` array and adds collection `metadata`.
Runs are ordered by generation and attempt.
Each compact-JSON response is limited to 4 MiB at complete-run boundaries.

## HTTP query parameters

| Parameter                 | Use                                                  |
|---------------------------|------------------------------------------------------|
| `slot`                    | Match one slot                                       |
| `phase`                   | Match any supplied phase; may be repeated            |
| `labelSelector`           | Match a Kubernetes-style label selector              |
| `limit`                   | Task or TaskRun page size; default `100`, maximum `1000` |
| `continue`                | Resume the matching Task or TaskRun snapshot         |
| `watch`                   | Select watch mode with `true` or `1`                 |
| `resourceVersion` (read)  | Start a watch at an opaque collection version        |
| `uid`                     | Apply or delete precondition                         |
| `resourceVersion` (write) | Apply or delete version precondition on write routes |

Filters are combined with AND.
Repeated `phase` values are combined with OR.
Unknown parameters are rejected.
Singleton parameters cannot be repeated.

`limit` and `continue` are not accepted in watch mode.
Read `resourceVersion` is accepted only in watch mode.
Write `resourceVersion` is accepted only by apply and delete routes.

## gRPC

The protobuf package is `solti.task.v1`.
Generated public types are available under `solti_api::grpc::wire`.

| RPC              | Shape            | Handler operation   |
|------------------|------------------|---------------------|
| `CreateTask`     | Unary            | Create              |
| `ApplyTask`      | Unary            | Apply               |
| `GetTask`        | Unary            | Get                 |
| `ListTasks`      | Unary            | List                |
| `WatchTasks`     | Server streaming | Watch               |
| `ListTaskRuns`   | Unary            | Paginated run history |
| `DeleteTask`     | Unary            | Delete              |
| `StreamTaskLogs` | Server streaming | Live output         |

Use the generated client directly:

```rust,no_run
use solti_api::grpc::wire::{
    ListTasksRequest, TaskPhase, TaskServiceClient,
};

async fn list_running() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = TaskServiceClient::connect("http://127.0.0.1:50052").await?;
    let page = client
        .list_tasks(ListTasksRequest {
            slot: Some("jobs".into()),
            phases: vec![TaskPhase::Running as i32],
            limit: 100,
            label_selector: "app=batch".into(),
            r#continue: String::new(),
        })
        .await?
        .into_inner();

    println!("tasks: {}", page.tasks.len());
    Ok(())
}
```

Every encoded and decoded gRPC message is limited to `MAX_REQUEST_BYTES`.
`ListTasks` and `ListTaskRuns` proactively keep their protobuf responses within that limit.
One Task or TaskRun that cannot fit returns gRPC `ResourceExhausted`.

## Create and apply

Create requires a new `metadata.name`.
Apply is an unconditional upsert when no precondition is supplied.

Apply and delete support two optional preconditions:

- `uid` checks resource identity;
- `resourceVersion` checks the current stored version.

HTTP carries them as query parameters.
gRPC carries them in `WritePreconditions`.
A mismatch returns a structured conflict.

With `SupervisorApiAdapter`, a successful create or apply commits desired state immediately.
Runtime reconciliation continues in `solti-core`.
Read `status.conditions[type=Reconciled]` to observe the result for that generation.

The adapter uses core retained Task admission.
The default limits are 1024 current Tasks and 256 MiB of aggregate retained
TaskManifest bytes. The byte budget measures only compact canonical
`TaskManifest` JSON and is independent of the current Task count.

A new name or unchecked missing apply is rejected atomically when either limit
would be exceeded. Applying an existing Task remains allowed by the count
limit. A positive-growth apply is rejected when it would exceed the byte
budget. A shrinking or no-op apply remains allowed. Admission does not evict
Tasks.

Admission rejection returns HTTP `429` with
`Status.reason=TooManyRequests`, or gRPC `ResourceExhausted`. The HTTP response
has no `Retry-After` header.

## Pagination and watches

Task lists use snapshot-consistent continuation pagination.

1. Send filters and `limit` without a continuation.
2. Read the returned opaque continuation token.
3. Send the token with the same filters.
4. Continue until no token is returned.

Every page keeps the first page's resource version.
The count limit is a ceiling. The 4 MiB native response limit can produce a smaller page.
Changing filters while using a continuation is rejected.
An expired snapshot returns HTTP `410 Gone` or gRPC `OutOfRange`.

With `SupervisorApiAdapter`, an absent watch resource version or `"0"` emits current matching tasks as `Added`.
The stream then emits live changes.
A specific version replays later retained changes before live delivery.

The bundled core admits 256 concurrent Task watches by default. Initial and
replay buffers share a 64 MiB aggregate compact Task JSON budget. Saturated
watch admission returns HTTP `429` with `reason=TooManyRequests`, or gRPC
`ResourceExhausted`. Rejection does not evict or terminate an existing watch.
Lag recovery waits for replay capacity and retains no replay payload while
waiting. Live events and events already yielded to the client are outside this
internal retained-payload budget.

HTTP watch events are newline-delimited JSON documents.
Abbreviated documents look like this:

```json
{"type":"ADDED","object":{"apiVersion":"solti.io/v1","kind":"Task"}}
{"type":"MODIFIED","object":{"apiVersion":"solti.io/v1","kind":"Task"}}
```

The event types are `ADDED`, `MODIFIED`, and `DELETED`.
A stream error is encoded as one final `ERROR` document.
The HTTP stream closes after that document.
gRPC reports the stream error as a status.

## Live output

HTTP uses Server-Sent Events.
The event names are:

- `chunk`;
- `run-started`;
- `run-finished`;
- `lagged`.

gRPC uses the matching `StreamTaskLogsResponse` oneof.
Output lines are raw bytes in protobuf.
HTTP JSON encodes those bytes as base64.
`OutputChunk.truncated` is true when `line` is an exact retained prefix and
source bytes were omitted. False is omitted from the HTTP JSON shape.

The stream is live-only and lossy.
It does not persist or replay output.
`Lagged` reports both the number of events and retained line bytes missed by a
slow subscriber. Lag metadata never modifies raw `line` bytes.
One subscription can span later attempts of the same task generation.
`SupervisorApiAdapter` filters an opened stream to the generation visible at subscription time.

## Public workload boundary

`Embedded` workloads are in-process SDK values.
They have no public HTTP or gRPC representation.

The transports reject `Embedded` input.
They also reject an `Embedded` value returned by a custom handler.
`SupervisorApiAdapter` hides embedded tasks, watches, history, output, apply, and delete operations.

Extension workloads remain public.
Their GVK and JSON object are preserved across both transports.

Chain is transported through this extension contract:

| Transport | Chain encoding |
|-----------|----------------|
| HTTP | `Task.spec.workload` uses `apiVersion: chain.solti.io/v1alpha1`, `kind: Chain`, and the Chain object in `spec` |
| gRPC | `TaskWorkload` uses the same GVK and carries the serialized Chain spec in `ExtensionTask.spec.raw` |

The API does not create separate resources for Chain steps. Status, run
history, and live output continue to describe the one outer Task. Chain graph
validation happens when the registered Chain runner builds the workload during
core reconciliation.

## Authentication and authorization

Task API authentication is disabled until `with_auth` or `with_authenticator` is called.

```rust,no_run
use std::sync::Arc;

use solti_api::{ApiHandler, HttpApi};
use solti_model::Token;

fn secured_router<H: ApiHandler>(handler: Arc<H>) -> solti_api::axum::Router {
    HttpApi::new(handler)
        .with_auth(Token::new("agent-secret").expect("valid token"))
        .router()
}
```

HTTP expects `Authorization: Bearer <token>`.
gRPC expects the same value in `authorization` metadata.
The scheme is case-insensitive.
An invalid credential is rejected before the handler.

`with_auth` is the static shared-token convenience path.
A valid token creates an authenticated identity without an individual subject.
Use`with_authenticator` to validate application credentials and return an `ApiIdentity` with a subject and application-owned attributes.

Both transports also accept an `ApiAuthorizer`:

```rust,no_run
use std::sync::Arc;

use async_trait::async_trait;
use solti_api::{
    ApiAuthorizer, ApiError, ApiHandler, AuthorizationRequest, HttpApi, TaskOperation,
};
use solti_model::Token;

struct ReadOnlyPolicy;

#[async_trait]
impl ApiAuthorizer for ReadOnlyPolicy {
    async fn authorize(&self, request: AuthorizationRequest<'_>) -> Result<(), ApiError> {
        if matches!(
            request.operation(),
            TaskOperation::Get
                | TaskOperation::List
                | TaskOperation::Watch
                | TaskOperation::ListRuns
                | TaskOperation::StreamLogs
        ) {
            Ok(())
        } else {
            Err(ApiError::Forbidden("read-only identity".into()))
        }
    }
}

fn read_only_router<H: ApiHandler>(handler: Arc<H>) -> solti_api::axum::Router {
    HttpApi::new(handler)
        .with_auth(Token::new("agent-secret").expect("valid token"))
        .with_authorizer(Arc::new(ReadOnlyPolicy))
        .router()
}
```

The authorizer receives the identity, exact `TaskOperation`, and either a validated manifest, one task name, or the Task collection.
A normal denial uses `ApiError::Forbidden`, which maps to HTTP `403` and gRPC `PermissionDenied`.

List and Watch authorization covers the collection operation.
The hook does not filter returned items or watch events.
Tenant or row-level visibility needs a separate scoped-handler design outside this hook.
Stream authorization is checked when the stream is opened, not for every event.

If no Task API authenticator is configured, application middleware can insert an `ApiIdentity` into request extensions before the Task API runs.
Solti does not define users, roles, tenants, RBAC rules, or policy storage.

## TLS

`grpc-tls` converts `solti_tls::ServerTlsConfig` for tonic:

```rust,no_run
use solti_api::to_tonic_server_tls;
use solti_tls::{ServerTlsConfig, TlsIdentity, TrustRoots};

fn load_tls() -> Result<
    solti_api::tonic::transport::ServerTlsConfig,
    solti_tls::TlsError,
> {
    let config = ServerTlsConfig::new(TlsIdentity::from_pem_files(
        "/etc/solti/tls/server.crt",
        "/etc/solti/tls/server.key",
    ))
    .require_client_auth(TrustRoots::from_pem_file(
        "/etc/solti/tls/clients-ca.crt",
    ));

    to_tonic_server_tls(config)
}
```

Client roots make client certificates mandatory.
HTTP TLS is configured by the server that hosts the returned axum router.
Bearer authentication and TLS are independent.

## Metrics

Both builders accept one `ApiMetricsHandle` through `with_metrics`.
The default backend does nothing.

The backend receives:

- transport;
- method;
- route or RPC path;
- status code;
- duration;
- in-flight changes.

HTTP paths use route templates instead of task names.
gRPC paths use the full service and method.
Streaming requests remain in flight until the body ends, fails, or is dropped.

`solti-prometheus` provides the Prometheus implementation.

## Errors

| `ApiError`                | HTTP                         | gRPC                |
|---------------------------|------------------------------|---------------------|
| `InvalidRequest`          | `400 Bad Request`            | `InvalidArgument`   |
| `Unauthenticated`         | `401 Unauthorized`           | `Unauthenticated`   |
| `Forbidden`               | `403 Forbidden`              | `PermissionDenied`  |
| `AlreadyExists`           | `409 Conflict`               | `AlreadyExists`     |
| `Conflict`                | `409 Conflict`               | `Aborted`           |
| `TaskNotFound`            | `404 Not Found`              | `NotFound`          |
| `NotFound`                | `404 Not Found`              | `NotFound`          |
| `MethodNotAllowed`        | `405 Method Not Allowed`     | `Unimplemented`     |
| `UnsupportedMediaType`    | `415 Unsupported Media Type` | `InvalidArgument`   |
| `PayloadTooLarge`         | `413 Payload Too Large`      | `ResourceExhausted` |
| `ResourceExhausted`       | `429 Too Many Requests`      | `ResourceExhausted` |
| `ResourceVersionExpired`  | `410 Gone`                   | `OutOfRange`        |
| `Unavailable`             | `503 Service Unavailable`    | `Unavailable`       |
| `Internal`                | `500 Internal Server Error`  | `Internal`          |

HTTP errors use a Kubernetes-style `Status` body.
`ResourceExhausted` uses the HTTP reason `TooManyRequests` and does not add
`Retry-After`.
Write conflicts include machine-readable causes.
gRPC write conflicts encode `WriteConflictDetails` in status details.
Internal failures are logged by stable category and hidden from clients.
The transport boundary does not write the diagnostic string to logs.

## Examples

### Internal examples

These examples exercise the public transport and adapter contracts.
They do not assemble discovery, observability, or concrete execution backends.
Each example starts with a text flow diagram, then explains its request, conversion, and result.

Start with the HTTP/JSON contract:

```bash
cargo run -p solti-api --example http_contract --features http
```

| Example                                         | Features       | What it shows                                                          |
|-------------------------------------------------|----------------|------------------------------------------------------------------------|
| [http_contract.rs](examples/http_contract.rs)   | `http`         | Authentication, CRD JSON, generated OpenAPI, and Server-Sent Events.   |
| [grpc_contract.rs](examples/grpc_contract.rs)   | `grpc`         | Generated client, bearer metadata, unary calls, and server streaming.  |
| [core_adapter.rs](examples/core_adapter.rs)     | `core-adapter` | Public workload filtering between `ApiHandler` and `solti-core`.       |

Run the remaining examples explicitly:

```bash
cargo run -p solti-api --example grpc_contract --features grpc
cargo run -p solti-api --example core_adapter --features core-adapter
```

The gRPC example binds one loopback listener.
No request leaves the local machine.

### Full examples

Application-level compositions live in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
They combine the API transports with concrete runners, core supervision, discovery, and observability.
