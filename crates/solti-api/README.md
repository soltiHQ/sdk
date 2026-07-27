# solti-api

HTTP/JSON and gRPC transports for the Solti task supervisor SDK.

Both transports delegate task operations to `ApiHandler`. The optional
`SupervisorApiAdapter` connects that boundary to `solti-core`.

HTTP exposes Kubernetes-shaped `Task`, `TaskList`, and `Status` resources under
`/apis/solti.io/v1`. gRPC uses the versioned protobuf contract in
`proto/solti/task/v1`.

## Features

| Feature        | Capability                                      |
|----------------|-------------------------------------------------|
| `core-adapter` | `SupervisorApiAdapter` for `solti-core`         |
| `grpc`         | tonic server and generated `grpc::v1` API       |
| `grpc-tls`     | `solti-tls` adapter for tonic; implies `grpc`   |
| `http`         | axum router with the CRD JSON representation    |

No feature is enabled by default.

```rust,ignore
use std::sync::Arc;

use solti_api::{GrpcApi, HttpApi, SupervisorApiAdapter};

let handler = Arc::new(SupervisorApiAdapter::new(supervisor));
let http = HttpApi::new(Arc::clone(&handler)).router();
let grpc = GrpcApi::new(handler).server();
```

## List Pagination

Task lists use snapshot-consistent continuation pagination.

- Send filters and `limit` for the first page.
- Read `metadata.continue` over HTTP or `continue` over gRPC.
- Send that token with the same filters for the next page.
- Every page keeps the first page's `resourceVersion`.
- `remainingItemCount` is present while more matching items remain.

The token is opaque. An invalid token is a bad request. A snapshot that has
left retained history returns HTTP `410 Gone` or gRPC `OutOfRange`.

The crate documentation is the source of truth for the Rust API and transport
semantics.
