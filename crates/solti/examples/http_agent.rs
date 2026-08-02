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
//! Run with `cargo run -p solti --example http_agent --features api-core-adapter,api-http,exec-subprocess`.

use std::sync::Arc;

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(ADDRESS).await?;

    let mut runner_router = RunnerRouter::new();
    register_subprocess_runner(&mut runner_router, "default")?;

    let supervisor = Arc::new(SupervisorApi::builder(runner_router).start().await?);
    let server_result = serve_http(Arc::clone(&supervisor), listener).await;
    let shutdown_result = supervisor.shutdown().await;

    server_result?;
    shutdown_result?;
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
    println!("Press Ctrl-C to stop");

    serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to install Ctrl-C handler: {error}");
    }
}
