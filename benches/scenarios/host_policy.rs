//! Opt-in production-library Linux host-policy lifecycle.

#[path = "support/process.rs"]
mod process;

#[cfg(target_os = "linux")]
mod linux {
    use super::process;
    use criterion::{BenchmarkId, Criterion, Throughput};
    use solti_benches::{
        fixtures::{RUNTIMES, WAIT_BOUND},
        report::{CaseFamily, print_suite_header, record_case},
    };
    use solti_exec::{
        host::{CgroupLimits, HostProcessPolicy, SeccompPolicy, SecurityConfig},
        subprocess::{SubprocessBackendConfig, register_subprocess_runner_with_backend},
    };
    use solti_runner::RunnerRouter;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use taskvisor::TaskContext;

    const POLICY: CaseFamily = CaseFamily::lifecycle(
        "host_policy/linux/lifecycle", "LINUX HOST POLICY · REAL PROCESS", "completed policy attempt", "completed policy attempts",
        "built task spawn through host-policy preparation, real child policy read-back, output drain, reap and cgroup cleanup",
        "runtime, explicit cgroup delegation, runner/policy and task build; post-attempt cgroup disappearance assertion and terminal runner shutdown",
    ).without_lifecycle_interpretation();

    pub fn bench(c: &mut Criterion) {
        if std::env::var("SOLTI_BENCH_LINUX_HOST").as_deref() != Ok("1") {
            eprintln!(
                "Linux host-policy benchmark skipped: requires SOLTI_BENCH_LINUX_HOST=1 and an explicitly delegated cgroup v2 parent"
            );
            return;
        }
        let parent = std::path::PathBuf::from(std::env::var("SOLTI_BENCH_CGROUP_PARENT").expect(
            "set SOLTI_BENCH_CGROUP_PARENT to an explicitly delegated writable cgroup v2 parent",
        ))
        .canonicalize()
        .expect("resolve delegated cgroup parent");
        assert!(
            parent.join("cgroup.controllers").is_file(),
            "cgroup v2 parent required"
        );
        let enabled = std::fs::read_to_string(parent.join("cgroup.subtree_control"))
            .expect("delegated subtree controls");
        assert!(
            enabled.split_whitespace().any(|name| name == "pids"),
            "the delegated parent must enable pids for children"
        );
        print_suite_header("host_policy");
        let mut group = c.benchmark_group(POLICY.group_id);
        group.throughput(Throughput::Elements(1));
        for &(rt_name, rt_fn) in &RUNTIMES {
            for label in ["unrestricted", "cgroup_pids8", "cgroup_pids8_seccomp"] {
                group.bench_function(BenchmarkId::new(rt_name, label), |b| {
                    record_case(POLICY, rt_name, Some(label.to_owned()));
                    let rt = rt_fn();
                    let output = Arc::new(process::RecordingOutput::default());
                    let mut router = RunnerRouter::new().with_output_publisher(output.clone());
                    let mut policy = HostProcessPolicy::new();
                    if label != "unrestricted" {
                        policy = policy
                            .with_cgroups(CgroupLimits {
                                pids: Some(8),
                                ..Default::default()
                            })
                            .with_cgroup_parent(parent.clone());
                    }
                    if label.ends_with("seccomp") {
                        policy = policy.with_security(SecurityConfig {
                            seccomp: SeccompPolicy::DenyHostControl,
                            ..Default::default()
                        });
                    }
                    let runner = register_subprocess_runner_with_backend(
                        &mut router,
                        "host-policy",
                        SubprocessBackendConfig::new().with_host_process_policy(policy),
                    )
                    .expect("host policy runner");
                    let args = if label == "unrestricted" {
                        vec!["quiet".into()]
                    } else {
                        vec![
                            "policy".into(),
                            parent.to_str().unwrap().to_owned(),
                            if label.ends_with("seccomp") {
                                "seccomp"
                            } else {
                                "cgroup"
                            }
                            .into(),
                        ]
                    };
                    let built = rt
                        .block_on(router.build(&process::task(
                            "host-policy-task",
                            process::command_workload(args),
                        )))
                        .expect("build host-policy task");
                    b.iter_custom(|iters| {
                        rt.block_on(async {
                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                let previous = output.snapshot();
                                let start = Instant::now();
                                tokio::time::timeout(
                                    WAIT_BOUND,
                                    built.task().spawn(TaskContext::detached()),
                                )
                                .await
                                .expect("host-policy attempt deadline")
                                .expect("host-policy child and cleanup");
                                process::wait_clean(&runner).await;
                                total += start.elapsed();
                                let observed = output.snapshot();
                                if label == "unrestricted" {
                                    assert_eq!(observed.done, previous.done + 1);
                                } else {
                                    assert_eq!(observed.policy_ok, previous.policy_ok + 1);
                                    let cgroup =
                                        observed.cgroup.expect("child reported its actual cgroup");
                                    assert_eq!(cgroup.parent(), Some(parent.as_path()));
                                    assert!(
                                        !cgroup.exists(),
                                        "attempt cgroup still exists after cleanup"
                                    );
                                }
                            }
                            total
                        })
                    });
                    drop(built);
                    drop(router);
                    rt.block_on(process::shutdown(&runner));
                });
            }
        }
        group.finish();
    }
}

fn benchmarks(c: &mut criterion::Criterion) {
    #[cfg(target_os = "linux")]
    linux::bench(c);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = c;
        eprintln!("Linux host-policy benchmarks skipped: this host is not Linux");
    }
}

criterion::criterion_group!(benches, benchmarks);

fn main() {
    if !process::maybe_child() {
        solti_benches::report::benchmark_main("host_policy", benches);
    }
}
