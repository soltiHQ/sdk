//! # HTTP agent: serve the Task API
//!
//! This example is a minimal HTTP agent binary.
//! It combines the subprocess runner, core supervisor, and HTTP Task API.
//!
//! The server listens on `127.0.0.1:8085`.
//! Task routes start at `HTTP_API_ROOT`.
//! `/openapi.json` serves the OpenAPI document generated from those routes.
//!
//! The OpenAPI endpoint is owned by this binary.
//! It is not included in the Task API document that it serves.
//! This example has no authentication, TLS, discovery, metrics, or tracing.
//! Ctrl-C stops HTTP intake and then joins the supervisor shutdown.
//!
//! ```text
//! HTTP client
//!      │ JSON Task API
//!      ▼
//! HttpApi ──► SupervisorApiAdapter ──► SupervisorApi
//!                                             ▼
//!                                      RunnerRouter
//!                                             ▼
//!                                    subprocess runner
//!
//! GET /openapi.json ──► generated contract for the mounted routes
//! ```
//!
//! Run with `cargo run -p solti --example agent_http --features api-core-adapter,api-http,exec-subprocess`.

use std::{sync::Arc, time::Duration};

use solti::{
    api::{
        HTTP_API_ROOT, HttpApi, HttpApiParts, SupervisorApiAdapter,
        axum::{routing::get, serve},
    },
    core::SupervisorApi,
    exec::subprocess::register_subprocess_runner,
    runner::RunnerRouter,
};
use tokio::net::TcpListener;

const ADDRESS: &str = "127.0.0.1:8085";

const HTTP_COMMANDS: &str = r#"
[commands] Run these from a second terminal.

TASKS='__TASKS__'
MANIFEST='/tmp/solti-http-task.json'

# Prepare one valid Subprocess Task manifest.
cat >"$MANIFEST" <<'JSON'
{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": {
    "name": "http-demo",
    "labels": {
      "example": "http-agent"
    }
  },
  "spec": {
    "slot": "http-example",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Subprocess",
      "spec": {
        "mode": {
          "command": {
            "command": "/bin/sh",
            "args": [
              "-c",
              "for i in $(seq 1 30); do sleep 2; echo tick=$i; done"
            ]
          }
        },
        "failOnNonZero": true
      }
    },
    "timeout": 90000,
    "restart": {
      "type": "never"
    },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 30000,
      "factor": 2.0
    },
    "admission": "dropIfRunning"
  }
}
JSON

# Create the task.
curl -sS -X POST \
  -H 'content-type: application/json' \
  --data-binary @"$MANIFEST" \
  "$TASKS"

# Stream live output while the one-minute task is running.
curl -sS -N -H 'accept: text/event-stream' \
  "$TASKS/http-demo/logs"

# Read the resource and a filtered collection page.
curl -sS "$TASKS/http-demo"
curl -sS "$TASKS?slot=http-example&limit=10"

# Apply the same desired state. This apply is a no-op.
curl -sS -X PUT \
  -H 'content-type: application/json' \
  --data-binary @"$MANIFEST" \
  "$TASKS/http-demo"

# Read retained attempt history after the command finishes.
curl -sS "$TASKS/http-demo/runs"

# Watch current objects and later changes. This runs until Ctrl-C.
curl -sS -N "$TASKS?watch=true&resourceVersion=0"

# Delete the resource and its retained history.
curl -i -X DELETE "$TASKS/http-demo"

# Read the generated OpenAPI document.
curl -sS '__OPENAPI__'
"#;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        r#"
solti: HTTP task agent

  HTTP/JSON ──► HttpApi ──► SupervisorApiAdapter ──► core
                                                        ▼
                                                 subprocess runner

  OpenAPI is generated from the same routes served by this process.
"#
    );
    println!(
        "[purpose] Assemble a runnable HTTP agent from API, core, runner, model, and exec crates."
    );
    let listener = TcpListener::bind(ADDRESS).await?;

    let mut runner_router = RunnerRouter::new();
    let subprocess_runner = register_subprocess_runner(&mut runner_router, "default")?;
    println!("[runner] Registered the built-in Subprocess GVK as runner=default.");

    let supervisor = Arc::new(SupervisorApi::builder(runner_router).start().await?);
    let server_result = serve_http(Arc::clone(&supervisor), listener).await;
    let shutdown_result = supervisor.shutdown().await;
    let finalizer_result = subprocess_runner.shutdown(Duration::from_secs(5)).await;

    server_result?;
    shutdown_result?;
    finalizer_result?;
    Ok(())
}

async fn serve_http(
    supervisor: Arc<SupervisorApi>,
    listener: TcpListener,
) -> Result<(), Box<dyn std::error::Error>> {
    let handler = Arc::new(SupervisorApiAdapter::new(supervisor));
    let HttpApiParts { router, openapi } = HttpApi::new(handler).build();

    let openapi = serde_json::to_string_pretty(&openapi)?;
    let app = router.route(
        "/openapi.json",
        get(move || {
            let openapi = openapi.clone();
            async move { ([("content-type", "application/json")], openapi) }
        }),
    );

    println!("HTTP Task API: http://{ADDRESS}{HTTP_API_ROOT}");
    println!("OpenAPI: http://{ADDRESS}/openapi.json");
    print_http_commands();
    println!("[shutdown] Press Ctrl-C in the agent terminal to stop.");

    serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn print_http_commands() {
    let commands = HTTP_COMMANDS
        .replace(
            "__TASKS__",
            &format!("http://{ADDRESS}{HTTP_API_ROOT}/tasks"),
        )
        .replace("__OPENAPI__", &format!("http://{ADDRESS}/openapi.json"));
    println!("{commands}");
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to install Ctrl-C handler: {error}");
    }
}
