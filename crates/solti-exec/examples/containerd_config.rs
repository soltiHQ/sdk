//! # Containerd configuration
//!
//! The native adapter targets containerd 2.x through one explicit Unix socket.
//! Configuration does not discover or start a daemon.
//!
//! This example shows:
//!
//! - endpoint, namespace, snapshotter, and OCI runtime selection;
//! - image platform selection;
//! - isolated and host network modes;
//! - control, transfer, and cleanup deadlines;
//! - the explicit startup connection boundary.
//!
//! The default run prints configuration without contacting containerd.
//! Add `--connect` when a compatible local daemon is available.
//!
//! Run with `cargo run -p solti-exec --example containerd_config --features containerd`.
//! Connect with `cargo run -p solti-exec --example containerd_config --features containerd -- --connect`.

use std::time::Duration;

use solti_exec::container::containerd::{
    ContainerNetwork, ContainerPlatform, ContainerdConfig, ContainerdEngine,
};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-exec: native containerd 2.x setup

  final binary configuration
      ├── Unix socket
      ├── containerd namespace
      ├── snapshotter + OCI runtime
      ├── OCI image platform
      ├── network: None | Host
      ├── registry hosts directory
      ├── shared I/O root
      └── operation deadlines
                    ▼
            ContainerdConfig
                    │ explicit connect
                    ▼
        ContainerdEngine::connect()
                    ├──► validate configuration
                    ├──► connect to endpoint
                    └──► require containerd major version 2

  Network::None creates an empty network namespace without CNI.
  Network::Host shares the host network namespace explicitly.
"#;

fn print_config(label: &str, config: &ContainerdConfig) {
    let platform = config.platform();
    println!(
        "[{label}] socket={}, namespace={}, snapshotter={}, runtime={}.",
        config.socket().display(),
        config.namespace(),
        config.snapshotter(),
        config.runtime(),
    );
    println!(
        "[{label}] platform={}/{}/{}, network={:?}, registryHosts={:?}, ioRoot={}.",
        platform.os(),
        platform.architecture(),
        platform.variant(),
        config.network(),
        config.registry_host_dir(),
        config.io_root().display(),
    );
    println!(
        "[{label}] control={:?}, transfer={:?}, cleanup={:?}.",
        config.control_timeout(),
        config.transfer_timeout(),
        config.cleanup_timeout(),
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Make the final binary's containerd endpoint, plugins, network boundary, and deadlines explicit."
    );

    let isolated = ContainerdConfig::new(
        "/run/containerd/containerd.sock",
        "solti",
        "overlayfs",
        "io.containerd.runc.v2",
    )
    .with_platform(ContainerPlatform::host_linux())
    .with_network(ContainerNetwork::None)
    .with_registry_host_dir("/etc/containerd/certs.d")
    .with_io_root("/run/solti/containerd-io")
    .with_control_timeout(Duration::from_secs(20))
    .with_transfer_timeout(Duration::from_secs(5 * 60))
    .with_cleanup_timeout(Duration::from_secs(45));
    let host = isolated.clone().with_network(ContainerNetwork::Host);

    print_config("isolated", &isolated);
    println!(
        "[isolated] The adapter creates a network namespace but does not add an interface, address, route, DNS, NAT, bridge, or CNI."
    );
    print_config("host", &host);
    println!("[host] The container shares the host network namespace.");

    let connect = std::env::args()
        .skip(1)
        .any(|argument| argument == "--connect");
    if connect {
        println!("[connect] Connecting to the configured endpoint and checking containerd 2.x.");
        let engine = ContainerdEngine::connect(isolated).await?;
        let info = engine.probe().await?;
        println!(
            "[connect] Accepted engine={} version={}.",
            info.name(),
            info.version(),
        );
        println!("\nResult: configuration and the live containerd endpoint passed startup checks.");
    } else {
        println!(
            "[connect] Skipped. Pass --connect only when this socket and containerd 2.x are available."
        );
        println!(
            "\nResult: both network variants are explicit; no daemon was discovered, started, or contacted."
        );
    }
    Ok(())
}
