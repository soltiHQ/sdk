//! Opt-in integration test for one complete public containerd 2.x lifecycle.
//!
//! The test is compiled with `containerd` and ignored by default. An explicit
//! run also requires `SOLTI_TEST_CONTAINERD=1`. The lane owns daemon
//! provisioning, image availability, privileges, and shared I/O path
//! configuration.

#![cfg(feature = "containerd")]

use std::sync::{Arc, Mutex};

use solti_exec::container::containerd::{ContainerNetwork, ContainerdConfig, ContainerdEngine};
use solti_exec::container::register_container_runner;
use solti_model::{ContainerSpec, OutputEvent, Task, TaskEnv, TaskId, TaskSpec, TaskWorkload};
use solti_runner::{OutputPublisher, OutputPublisherHandle, OutputSink, RunnerRouter};
use taskvisor::TaskContext;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct RecordingOutput {
    events: Arc<Mutex<Vec<OutputEvent>>>,
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(&self, _task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        let events = Arc::clone(&self.events);
        Some(OutputSink::new(generation, attempt, move |event| {
            events
                .lock()
                .expect("containerd integration output lock")
                .push(event);
        }))
    }
}

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an explicitly provisioned Linux containerd 2.x daemon"]
async fn public_container_runner_completes_one_real_containerd_attempt() -> TestResult {
    if std::env::var("SOLTI_TEST_CONTAINERD").as_deref() != Ok("1") {
        return Err(
            "set SOLTI_TEST_CONTAINERD=1 to contact a provisioned containerd 2.x daemon".into(),
        );
    }
    if !cfg!(target_os = "linux") {
        return Err("SOLTI_TEST_CONTAINERD=1 requires a Linux test host".into());
    }

    let socket = setting(
        "SOLTI_TEST_CONTAINERD_SOCKET",
        "/run/containerd/containerd.sock",
    );
    let namespace = setting("SOLTI_TEST_CONTAINERD_NAMESPACE", "solti-integration");
    let snapshotter = setting("SOLTI_TEST_CONTAINERD_SNAPSHOTTER", "overlayfs");
    let runtime = setting("SOLTI_TEST_CONTAINERD_RUNTIME", "io.containerd.runc.v2");
    let image = setting(
        "SOLTI_TEST_CONTAINERD_IMAGE",
        "docker.io/library/alpine:3.21",
    );
    let io_root = setting("SOLTI_TEST_CONTAINERD_IO_ROOT", "/tmp");
    let config = ContainerdConfig::new(socket, namespace, snapshotter, runtime)
        .with_network(ContainerNetwork::Host)
        .with_io_root(io_root);
    let engine = Arc::new(ContainerdEngine::connect(config).await?);

    let attempt = async {
        let info = engine.probe().await?;
        if info.name() != "containerd" {
            return Err(format!("unexpected engine name: {}", info.name()).into());
        }

        let output = Arc::new(RecordingOutput::default());
        let output_handle: OutputPublisherHandle = output.clone();
        let mut router = RunnerRouter::new().with_output_publisher(output_handle);
        register_container_runner(&mut router, "containerd-integration", Arc::clone(&engine))?;

        let mut env = TaskEnv::new();
        env.push("SOLTI_TEST_MESSAGE", "containerd-public-lifecycle");
        let workload = TaskWorkload::Container(ContainerSpec::new(
            image,
            Some(vec!["/bin/sh".into()]),
            vec!["-c".into(), "printf '%s\\n' \"$SOLTI_TEST_MESSAGE\"".into()],
            env,
        ));
        let spec = TaskSpec::builder("containerd-integration", workload, 120_000_u64).build()?;
        let task = Task::new("containerd-integration", spec)?;
        let attempt_timeout = std::time::Duration::from_millis(task.spec().timeout().as_millis());
        let built = router.build(&task).await?;
        tokio::time::timeout(attempt_timeout, built.task().spawn(TaskContext::detached()))
            .await
            .map_err(|_| "container attempt exceeded its declared TaskSpec timeout")??;

        let observed = output
            .events
            .lock()
            .expect("containerd integration output lock")
            .iter()
            .any(|event| {
                matches!(
                    event,
                    OutputEvent::Chunk(chunk)
                        if chunk.line.as_ref() == b"containerd-public-lifecycle"
                )
            });
        if !observed {
            return Err("container output did not contain the integration marker".into());
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    let shutdown = engine.shutdown().await;
    match (attempt, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(attempt), Err(shutdown)) => Err(format!(
            "container attempt failed: {attempt}; engine shutdown failed: {shutdown}"
        )
        .into()),
    }
}
