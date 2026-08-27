---
title: Serve the Task API
description: Connect HTTP or gRPC to a task backend and keep transport, reconciliation, and shutdown ownership explicit.
---

# Serve the Task API

`solti-api` exposes task operations to remote clients.
It validates requests and encodes responses. It does not store tasks, select a runner, or execute work.
Use `SupervisorApiAdapter` when the backend is the SDK supervisor. Implement `ApiHandler` when the application owns another backend.

## Connect the participants

| Participant | Why it is here |
|-------------|----------------|
| Application binary | Owns listeners, network access, credentials, server futures, and shutdown. |
| `solti-api` | Provides HTTP and gRPC transports, validation, access-control hooks, error mapping, and transport metrics. |
| `solti-model` | Defines manifests, stored Tasks, queries, preconditions, runs, and output events. |
| `solti-core` through `SupervisorApiAdapter` | Commits desired state and exposes reconciliation, collections, and live output. |
| `solti-runner` and the registered runner implementations | Route and build the workload after core accepts the desired state. |

```text
HTTP client ──► HttpApi ──┐
                         ├──► ApiHandler ──► SupervisorApiAdapter ──► solti-core
gRPC client ──► GrpcApi ──┘                                              │
                                                                         ▼
                                                              router and runners
```

The transports share domain operations, not wire encodings.
HTTP uses the model's CRD JSON. The documented gRPC service is `solti.task.v1.TaskService`.
The public behavior is specified in the handwritten [Task API contract](../crates/solti-api/CONTRACT.md).
One build exposes one Task API major; it does not host several major versions at once.

## Select features

No `solti-api` feature is enabled by default.

| Need | Direct `solti-api` feature | `solti` facade feature |
|------|---------------------------|------------------------|
| HTTP router | `http` | `api-http` |
| gRPC service | `grpc` | `api-grpc` |
| SDK supervisor backend | `core-adapter` | `api-core-adapter` |
| Tonic TLS adapter | `grpc-tls` | `api-grpc-tls` |

`grpc-tls` includes `grpc`. HTTP TLS belongs to the server that hosts the router.
Transport features do not register an execution backend.
See [installation](installation.md) and [routing and custom runners](routing-and-custom-runners.md).

## Build and host HTTP

Once the application has started a supervisor, build its public router:

```rust
use std::sync::Arc;
use solti_api::{HttpApi, HttpApiParts, SupervisorApiAdapter};
use solti_core::SupervisorApi;

fn build_http(supervisor: Arc<SupervisorApi>) -> HttpApiParts {
    let handler = Arc::new(SupervisorApiAdapter::new(supervisor));
    HttpApi::new(handler).build()
}
```

This requires `solti-api` features `http,core-adapter`.
`HttpApiParts.router` is an axum router. `HttpApiParts.openapi` is its in-memory OpenAPI 3.1 document.
Building them does not bind a socket or serve the document at a URL.
`HttpApi::router()` is the convenience form when the document is not needed.

The complete [HTTP agent example](../crates/solti/examples/agent_http.rs) performs these steps:

1. Bind `127.0.0.1:8085`.
2. Register the subprocess runner and retain its lifecycle handle.
3. Start `SupervisorApi` with that router.
4. Build `SupervisorApiAdapter` and `HttpApi`.
5. Add the binary-owned `/openapi.json` route and run `axum::serve`.
6. On Ctrl-C, finish the HTTP server, join supervisor shutdown, and join subprocess-runner cleanup.

Run it from the workspace root:

```sh
cargo run -p solti --example agent_http \
  --features api-core-adapter,api-http,exec-subprocess
```

The example has no authentication or TLS. Its execution behavior follows the [subprocess backend](subprocesses.md).
The printed live-output command needs the current Task UID; use the request shown in [Read live output](#read-live-output).

The application owns HTTP connection and stream draining.
`HttpApi` does not install signal handlers or shut down the supervisor.
Keep transport shutdown, [core shutdown](cancellation-and-shutdown.md), and runner-specific finalization as separate owned operations.

## Mount into an application router

Use `HttpApi::mount(app, &mut openapi)` to add the Task API to an Aide `ApiRouter`.
The application keeps its own state, metadata, routes, middleware, and OpenAPI document.
Call `finish_api` once after mounting every documented service.
The [mounting example](../crates/solti-api/README.md#contracts) shows the complete typed composition.

Task authentication, authorization, body limits, metrics, and fallbacks are scoped to `/apis/solti.io/v1`.
They do not automatically protect `/health`, `/openapi.json`, or another application route.
The generated Task schemas exclude the in-process `Embedded` workload without changing unrelated application schemas.

## Use the operation boundary

All paths below start with `/apis/solti.io/v1`.

| Operation | HTTP | Documented gRPC method |
|-----------|------|------------------------|
| Create | `POST /tasks` → `201` | `CreateTask` |
| Apply | `PUT /tasks/{name}` → `200` | `ApplyTask` |
| Get | `GET /tasks/{name}` | `GetTask` |
| List | `GET /tasks` | `ListTasks` |
| Watch | `GET /tasks?watch=true` | `WatchTasks` |
| Run history | `GET /tasks/{name}/runs` | `ListTaskRuns` |
| Cancel | `POST /tasks/{name}/cancel` → `204` | `CancelTask` |
| Delete | `DELETE /tasks/{name}` → `204` | `DeleteTask` |
| Live output | `GET /tasks/{name}/logs?taskUid={uid}` | `StreamTaskLogs` |

With the core adapter, create and apply return committed desired state.
Runner construction and runtime admission happen asynchronously afterward.
Observe the matching generation's `Reconciled` condition and execution phase to distinguish acceptance from execution.
See [reconciliation](reconciliation.md).

Create requires a new retained name. Apply without preconditions is an upsert.
Apply, cancel, and delete can require a matching `uid`, `resourceVersion`, or both.
HTTP carries these as query parameters. A failed precondition is a conflict, not a successful write.
The PUT path name must match `metadata.name`.

Cancel retains desired state and run history. It does not suppress later reconciliation.
Delete removes retained state after logical cancellation.
Neither response proves that force-aborted task code has physically exited.
See [managing tasks](managing-tasks.md) and [cancellation and shutdown](cancellation-and-shutdown.md).

## Walk collections and resume watches

Task lists support `slot`, repeated `phase`, `labelSelector`, `limit`, and `continue`.
Different filters combine with AND; repeated phases combine with OR.
An omitted or zero `limit` means 100 items, with a maximum of 1000.
Unknown parameters and repeated singleton parameters are rejected.

For a complete list:

1. Request the first page with the chosen filters.
2. Return `metadata.continue` unchanged with the same filters.
3. Stop when the continuation is absent.

The core adapter freezes the first page's snapshot and collection `resourceVersion`.
Concurrent writes do not move later pages to a new snapshot.
The count is a ceiling: the complete encoded response also has a 4 MiB limit.
One Task that cannot fit into an otherwise empty page returns resource exhaustion.

Run history has a separate snapshot and continuation chain.
Its `metadata.taskUid` remains pinned to the original Task incarnation across deletion and recreation.
Runs are ordered by generation and attempt; the frozen value of an active run does not change midway through pagination.

HTTP watches are newline-delimited JSON, not SSE and not one JSON array:

```text
GET /apis/solti.io/v1/tasks?watch=true&resourceVersion=0
```

An absent version or `0` emits current matches as `ADDED`, then live changes.
A retained specific version replays later changes before live delivery.
`ADDED`, `MODIFIED`, and `DELETED` describe membership in the selected collection.
In particular, `DELETED` can mean that a resource stopped matching a filter.

After exact-version replay or live delivery, clients can resume from the last
processed change's `metadata.resourceVersion`. Initial snapshot objects retain
their individual versions and arrive in name order; their versions are not a
snapshot-complete cursor. For a complete initial view, finish a paginated list
and start watching from its collection `resourceVersion`.
An expired position requires a fresh collection view; it cannot replay an unretained interval.
An error before streaming is an ordinary error response.
An error after the HTTP watch opens is one final `ERROR` document followed by stream closure.
`limit` and `continue` are not accepted in watch mode.

See [collections and watches](collections-and-watches.md) for core snapshot, history, and watch-admission limits.

## Read live output

Read the current Task first and use its exact `metadata.uid`:

```text
GET /apis/solti.io/v1/tasks/{name}/logs?taskUid={metadata.uid}
Accept: text/event-stream
```

A missing or invalid UID returns `400`. A valid non-current UID returns `404` before the stream opens.
The core adapter pins a successful subscription to that UID and the generation visible at opening.
It can span later attempts of that generation. It never retargets a recreated resource.

HTTP uses SSE event names `chunk`, `run-started`, `run-finished`, and `lagged`.
Each event carries the Task UID. Chunk bytes are base64, including non-UTF-8 output.
`truncated=true` means that only a source prefix was retained.
`lagged` reports missed events and retained line bytes.

Output is live-only and lossy. Opening the stream does not replay earlier output.
Run markers are observations, not ordering barriers for chunks or final-outcome guarantees.
Group chunks by UID, generation, attempt, and stream; use `seq` within that group.
See [output and history](output-and-history.md).

## Host gRPC

`GrpcApi::new(handler).server()` returns a service for an application-owned tonic server.
It can share the same handler with HTTP.
`WatchTasks` and `StreamTaskLogs` are server-streaming methods; other operations in the table are unary.
The documented message limit is 4 MiB.

The [Task API contract](../crates/solti-api/CONTRACT.md) defines gRPC errors, streams, and encoding separately from HTTP.
Use [TLS and authentication](tls-and-authentication.md) for the public tonic TLS adapter and shared access-control hooks.
The transport does not choose the listening address or own the binary's shutdown policy.

## Handle rejection and visibility

HTTP errors use a Kubernetes-style `Status` body.

| Response | Meaning |
|----------|---------|
| `400` / `415` / `413` | Invalid request, unsupported create/apply content type, or request body over 4 MiB. |
| `401` / `403` | Authentication failed or authorization denied the operation. |
| `404` | Public resource or route not found; hidden Embedded resources also appear absent. |
| `409` | Existing create target or failed write precondition; inspect the typed conflict causes. |
| `410` | Snapshot or watch position is no longer retained. |
| `429` | Retained-resource admission, watch admission, or an unrepresentable list item exhausted capacity. |
| `503` | Core operation admission is closed during shutdown. |
| `500` | Internal failure; clients receive the fixed `internal server error` message. |

Task API `429` responses do not include `Retry-After`.
The [error contract](../crates/solti-api/CONTRACT.md#errors) gives the matching gRPC status codes.

Built-in Subprocess, Container, and Wasm values have public representations.
This does not mean that a compatible runner is installed.
Application extension workloads are public, including Chain.
Embedded workloads are in-process only: transports reject their input and the core adapter hides their resources, runs, and output.

Custom `ApiHandler` implementations must preserve public visibility and the documented query/continuation contract.
The transports reject inconsistent pages or an Embedded value returned by a handler.
Core-specific reconciliation and snapshot behavior comes from `SupervisorApiAdapter`, not from the trait alone.

Source: [handler contract](../crates/solti-api/src/handler.rs), [core adapter](../crates/solti-api/src/adapter.rs), [HTTP implementation](../crates/solti-api/src/http.rs), and [public behavior](../crates/solti-api/CONTRACT.md).

## Continue with examples

- [HTTP contract](../crates/solti-api/examples/http_contract.rs): authentication, JSON, OpenAPI, and SSE through a small custom handler; no socket is opened. Run `cargo run -p solti-api --example http_contract --features http`.
- [Core adapter](../crates/solti-api/examples/core_adapter.rs): actual core state and public Embedded filtering. Run `cargo run -p solti-api --example core_adapter --features core-adapter`.
- [HTTP agent with discovery](../crates/solti/examples/agent_http_discovery.rs): connect the public API to the [discovery process](discovery.md).
- [Observability](observability.md): inject API metrics and keep application logs separate from task output.
