//! Shared composition for the two HTTP discovery examples.

use std::{env, future::IntoFuture, io, sync::Arc, time::Duration};

use solti::{
    api::{API_VERSION, HTTP_API_ROOT, HttpApi, SupervisorApiAdapter, axum::serve},
    core::SupervisorApi,
    discover::{
        AgentEndpoint, AgentEndpointType, ControlPlaneEndpoint, DISCOVERY_HTTP_SYNC_PATH,
        DiscoverConfig, DiscoveryTransport, MonotonicUptime, sync,
    },
    exec::subprocess::register_subprocess_runner,
    model::AgentId,
    runner::RunnerRouter,
};
use tokio::net::TcpListener;

pub type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub struct Defaults {
    pub agent_id: &'static str,
    pub agent_name: &'static str,
    pub control_plane: &'static str,
    pub api_address: &'static str,
    pub interval_ms: u64,
}

pub struct Settings {
    pub agent_id: String,
    agent_name: String,
    control_plane: String,
    api_address: String,
    advertise_url: Option<String>,
    interval_ms: u64,
}

impl Settings {
    pub fn from_env(defaults: Defaults) -> ExampleResult<Self> {
        fn value(name: &str, default: &str) -> Result<String, env::VarError> {
            match env::var(name) {
                Ok(value) => Ok(value),
                Err(env::VarError::NotPresent) => Ok(default.into()),
                Err(error) => Err(error),
            }
        }
        let advertise_url = match env::var("SOLTI_AGENT_ADVERTISE_URL") {
            Ok(value) => Some(base_url("SOLTI_AGENT_ADVERTISE_URL", &value)?),
            Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        };
        let agent_id = value("SOLTI_AGENT_ID", defaults.agent_id)?;
        AgentId::new(&agent_id)?;
        let agent_name = value("SOLTI_AGENT_NAME", defaults.agent_name)?;
        if agent_name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SOLTI_AGENT_NAME must not be empty",
            )
            .into());
        }
        Ok(Self {
            agent_id,
            agent_name,
            control_plane: base_url(
                "SOLTI_CONTROL_PLANE",
                &value("SOLTI_CONTROL_PLANE", defaults.control_plane)?,
            )?,
            api_address: value("SOLTI_AGENT_ADDR", defaults.api_address)?,
            advertise_url,
            interval_ms: defaults.interval_ms,
        })
    }
}

// These examples serve plaintext HTTP and use base URLs, not API route URLs.
fn base_url(name: &str, value: &str) -> ExampleResult<String> {
    let value = value.trim().trim_end_matches('/');
    let uri: solti::api::axum::http::Uri = value.parse()?;
    let host = uri.host().unwrap_or_default();
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || host.is_empty()
        || matches!(host, "0.0.0.0" | "::" | "[::]")
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        || uri.port_u16() == Some(0)
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be a reachable HTTP(S) base URL without credentials, a query or a fragment"),
        )
        .into());
    }
    Ok(value.into())
}

pub async fn run(settings: Settings) -> ExampleResult {
    let agent_id = AgentId::new(&settings.agent_id)?;
    let listener = TcpListener::bind(&settings.api_address).await?;
    let bound = listener.local_addr()?;
    let advertised = match settings.advertise_url {
        Some(address) => address,
        None if bound.ip().is_unspecified() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Wildcard bind requires SOLTI_AGENT_ADVERTISE_URL reachable from Podium",
            )
            .into());
        }
        None => format!("http://{bound}"),
    };
    let mut router = RunnerRouter::new();
    let subprocess_runner = register_subprocess_runner(&mut router, "default")?;
    let run_result = async {
        let capabilities = router.capabilities();
        println!(
            "[runner] Registered {} runner capability with {} workload GVK.",
            capabilities.runners().len(),
            capabilities.runners()[0].workload_types().len(),
        );
        let config = DiscoverConfig::builder(
            agent_id,
            settings.agent_name,
            AgentEndpoint::new(&advertised, AgentEndpointType::Http, API_VERSION)?,
            ControlPlaneEndpoint::new(&settings.control_plane, DiscoveryTransport::Http)?,
            capabilities,
            settings.interval_ms,
            format!(
                "http-agent@{}|control-plane={}|advertised={advertised}",
                env!("CARGO_PKG_VERSION"),
                settings.control_plane,
            ),
        )
        .build()?;
        let (manifest, task_ref) = sync(config, Arc::new(MonotonicUptime::new()))?;
        let discovery_name = manifest.name().clone();
        let supervisor = Arc::new(SupervisorApi::builder(router).start().await?);

        // Every exit after starting core joins both core and the subprocess runner.
        let serve_result = async {
            supervisor.create_embedded_task(manifest, task_ref).await?;
            println!("[agent] id={}", settings.agent_id);
            println!("[discovery] Supervised embedded task={discovery_name}.");
            println!(
                "[discovery] controlPlane={}, path={DISCOVERY_HTTP_SYNC_PATH}, interval={}ms.",
                settings.control_plane, settings.interval_ms,
            );
            println!("[api] Bound http://{bound}; advertised {advertised}{HTTP_API_ROOT}");
            println!("[api] Embedded discovery state is hidden by the public adapter.");
            println!("[shutdown] Ctrl-C or SIGTERM stops intake and supervised tasks.");

            let handler = Arc::new(SupervisorApiAdapter::new(Arc::clone(&supervisor)));
            let app = HttpApi::new(handler).router();
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
            let server = serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .into_future();
            tokio::pin!(server);
            let result = tokio::select! {
                result = &mut server => result,
                signal = shutdown_signal() => {
                    signal?;
                    let _ = shutdown_tx.send(());
                    let (drained, core_result) = tokio::join!(
                        tokio::time::timeout(Duration::from_secs(15), &mut server),
                        supervisor.shutdown_with_timeout(Duration::from_secs(10)),
                    );
                    core_result?;
                    drained.map_err(io::Error::other)?
                }
            };
            result?;
            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;
        let shutdown_result = supervisor
            .shutdown_with_timeout(Duration::from_secs(10))
            .await;
        serve_result?;
        shutdown_result?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    // Registering the runner starts owned workers; join them on setup failures too.
    let finalizer_result = subprocess_runner.shutdown(Duration::from_secs(5)).await;
    run_result?;
    finalizer_result?;
    println!("[shutdown] HTTP server, core, discovery and subprocess runner stopped.");
    Ok(())
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
