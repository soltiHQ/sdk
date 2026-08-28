# SDK process benchmarks

This suite measures SDK processes, including paths that cross crate boundaries.
It lives in the root `benches/` directory as the unpublished `solti-benches`
workspace package. Product crates do not own benchmark targets or depend on it.

The reporting wrapper follows Taskvisor: a machine/build header, grouped case
cards, named throughput units, and explicit **Boundary**, **Outside**, and
**Scope** descriptions. The wrapper is in [src/report.rs](src/report.rs).
Scenario code and controlled workloads live in [scenarios/](scenarios/).

## Run

From the SDK root, use the repository's shared Rust-task wrapper:

```bash
task rust:benchmark
```

Or run the same package directly with Cargo:

```bash
cargo bench -p solti-benches --benches --locked -- --quiet --color always
```

The Task wrapper uses the shared pinned Rust container. Direct Cargo uses the
local toolchain and host; these are different benchmark environments.

Default features select the portable suite: core, real subprocesses, a
controlled container engine, loopback HTTP/discovery/TLS, logging, and metrics.
These cases need no external server or container daemon. Network cases bind
ephemeral loopback ports. Subprocess cases use the benchmark executable as a
controlled child; Script cases also use the local shell. Logging cases start an
isolated child to install the SDK's process-global logger.

Build the selected suite without running it, or smoke-test every selected case:

```bash
cargo bench -p solti-benches --benches --no-run --locked
cargo bench -p solti-benches --benches --locked -- --test
```

Smoke mode checks execution and assertions. It is not a statistical measurement
and does not print measured performance cards.

Run the reporting and fixture regression tests separately:

```bash
cargo test -p solti-benches --lib --tests --locked
```

Select one target, then optionally filter by its Criterion case ID:

```bash
cargo bench -p solti-benches --no-default-features --bench reconciliation --locked
cargo bench -p solti-benches --no-default-features --bench reconciliation --locked -- 'reconciliation/apply/metadata/current_thread'
```

Use Criterion's baseline options with the same target and filter when comparing
revisions:

```bash
cargo bench -p solti-benches --no-default-features --bench reconciliation --locked -- 'reconciliation/apply/metadata/current_thread' --save-baseline before
cargo bench -p solti-benches --no-default-features --bench reconciliation --locked -- 'reconciliation/apply/metadata/current_thread' --baseline before
```

Run statistical comparisons sequentially on the same host, with the same
compiler, features, runtime, and workload parameters. The short smoke command
does not replace that comparison.

## Process scenarios

Each row is a benchmark target, not a product-crate boundary. The case's printed
Boundary and Outside fields specify exactly where its timer starts and stops.

| Target | Processes and variants | Feature |
|---|---|---|
| [lifecycle](scenarios/lifecycle.rs) | Cold supervisor startup through shutdown; warm create/success/delete cycles; retry with retained history; shared-slot Drop, Queue, and Replace | Base |
| [reconciliation](scenarios/reconciliation.rs) | No-op, metadata, guarded and conflicting apply; spec replacement; retry after failed build; retained-count/manifest-byte rejection; latest-wins bursts; global/per-runner build admission | Base |
| [collections](scenarios/collections.rs) | Task snapshot pagination with interleaved metadata writes; initial/replayed/live watch delivery; expired positions and watch admission; run-history pagination across retention eviction; terminal retention sweep | Base; `fixtures` adds explicit sweep |
| [execution](scenarios/execution.rs) | Command/Script build, reused attempt, and build-plus-attempt; ready process-tree cancel/delete/forced-drop through ownership release | `subprocess` |
| [output](scenarios/output.rs) | Real subprocess stdout/stderr with 0/1/4/8 subscribers; oversized chunks; delayed readers with event-ring or aggregate-byte pressure | `subprocess` |
| [chain](scenarios/chain.rs) | Build of all branches; successful steps, preserved failure, recovery and cancellation; outer retry from entry; real subprocess steps | Base; `subprocess` adds real child steps |
| [container](scenarios/container.rs) | Controlled engine success, start failure, nonzero exit, cancellation and forced drop; separately gated native containerd attempt | `container`; `containerd` adds native lane |
| [host_policy](scenarios/host_policy.rs) | Real Linux subprocess without optional controls, with cgroup pids limit, and with cgroup plus seccomp; child policy read-back and cleanup checks | `host-policy`, explicit host opt-in |
| [http](scenarios/http.rs) | Create-to-commit versus client-observed success; no-auth/bearer; snapshot traversal with writes; initial/replayed/live NDJSON watch; runner output through SSE | `http` |
| [discovery](scenarios/discovery.rs) | First supervised heartbeat; HTTP 503 followed by supervised retry; warm requests through a reused client | `discovery` |
| [tls](scenarios/tls.rs) | PEM configuration loading; fresh TCP/TLS handshake plus first exchange; established encrypted exchanges; TLS and mTLS | `tls` |
| [observability](scenarios/observability.rs) | Same task batch without/with metrics and with concurrent scrape; registry sizes and concurrent HTTP scrapes; saturated exporter rejection/recovery; SDK logger off/text/JSON | `observability` |
| [persistence](scenarios/persistence.rs) | State commits through callback delivery; blocked lossless state-queue admission and recovery; output publication while a lossy callback queue is blocked | Base |
| [shutdown](scenarios/shutdown.rs) | Empty supervisor; active cooperative tasks; mixed active tasks, blocked runner builds, pending state callbacks and watch closure | Base |

`portable` enables all table features except native `containerd` and
`host-policy`. Individual features can be selected with
`--no-default-features --features <names>`.

The HTTP runner is a controlled, serializable extension workload. It does not
send an Embedded TaskRef over HTTP. Discovery uses the actual SDK sync task and
a local controlled HTTP endpoint. TLS uses generated local certificates and
real loopback connections; certificate generation is outside the timed region.
No gRPC benchmark or protobuf-integration change is part of this suite.

Each TLS network case retains one listener across warm-up and samples. The cold
case still opens a new TCP connection and performs a full TLS handshake on every
iteration; session resumption is disabled. After the timed exchange, the server
closes first. Both peers verify TLS closure and TCP EOF before the server joins.
This keeps `TIME_WAIT` on the retained server port instead of consuming a new
local listening port per iteration. No connection retry or TCP tuning is used.

## Interpreting results

- `current_thread` and `multi_thread` use Tokio current-thread and four-worker
  runtimes. Runtime construction is outside steady-state timers.
- Cold, commit, reconciliation, query, output, and shutdown boundaries are
  separate cases. A desired-state acknowledgement is not task completion.
- An observed SDK terminal phase is not, by itself, proof of physical attempt
  cleanup. Cases ending at that observation use a neutral interpretation.
  Process-tree cases explicitly check stopped processes and released runner
  ownership. On Linux, a zombie descendant counts as stopped, not as reaped.
- Shared Embedded, controlled routed, and HTTP success fixtures explicitly use
  `Queue`. Terminal state and cancel/delete acknowledgements do not guarantee
  controller Idle before the next same-slot submission. The slot-policy cases
  explicitly select `DropIfRunning`, `Queue`, or `Replace`; the success fixtures
  are not a baseline for the default `DropIfRunning` policy.
- Throughput uses the case's named unit: commits, traversals, delivered changes,
  attempts, or another stated operation. A batch rate is the configured unit
  count divided by its measured duration. Amortized time per unit is not a
  per-request latency percentile or a host-independent capacity claim.
- Output pressure counts offered source lines, including loss. Watch fan-out
  counts delivered changes across subscribers. These are different quantities.
- State persistence cases use controlled callbacks, not a database. Their work
  parameter is synthetic callback work; it is not storage throughput. The
  controlled container engine creates no real container.
- Automatic task-attempt retry cases include their fixed 1 ms backoff.
  Discovery's first heartbeat also includes startup jitter. These configured
  waits are not subtracted. Manual apply after a failed build has no such backoff.
- Explicit setup, cleanup, and correctness checks follow each case's declared
  boundary. `iter_custom` excludes the stated untimed regions; it does not imply
  that all assertions or all coordination are outside the timer.
- Readiness uses observations and gates. The common 30-second timeout is a
  failure bound, not an SLA or a synchronization delay. Timeout diagnostics
  include the bounded operation's call site or the task's latest SDK state.
- Saturated-scrape setup and recovery can retry `503` outside the timer:
  client response completion does not prove the server released its last
  response-payload owner and scrape permit. Physical collector entry gates the
  measurement; the measured rejection remains one strict `503` request without
  retries. Early setup failure is observed alongside the collector gate.

The suite enables Taskvisor's `test-util` only in this unpublished package for
detached runner-attempt contexts. The optional `fixtures` feature enables core's
public test-util sweep entrypoint. That sweep case measures explicit retention
work, not the periodic worker's scheduling delay.

The report reads Criterion estimates only for cases executed in the current
invocation and checks that the results are fresh. It does not present old files
as a new measurement. `--test`, `--list`, profiling, and loaded-baseline modes do
not produce the custom measured summary. Criterion's raw results and HTML
reports remain available in its output directory. `CRITERION_HOME` selects an
explicit results directory; `CARGO_TARGET_DIR` is also supported.

`NO_COLOR` disables automatic color; `--color always` explicitly requests it.
`SOLTI_BENCH_CPU` can supply a CPU label when automatic detection is unavailable.

## Explicit Linux environments

Compiling a feature does not authorize a benchmark to use an external daemon or
host cgroup. These lanes additionally require their environment opt-in. The
portable suite neither provisions infrastructure nor enables these flags.

### Native containerd

Use an explicitly prepared Linux/containerd 2.x environment. Set all of:

- `SOLTI_BENCH_CONTAINERD=1`
- `SOLTI_BENCH_CONTAINERD_IMAGE`: the prepared image reference
- `SOLTI_BENCH_CONTAINERD_SOCKET`: daemon socket
- `SOLTI_BENCH_CONTAINERD_NAMESPACE`: dedicated benchmark namespace
- `SOLTI_BENCH_CONTAINERD_IO_ROOT`: writable I/O root visible at the same path to
  both processes; writable shared ancestors must have the sticky bit
- `SOLTI_BENCH_CONTAINERD_SNAPSHOTTER`: configured snapshotter
- `SOLTI_BENCH_CONTAINERD_RUNTIME`: configured OCI runtime

The image must provide `/bin/sh` and `printf`. Its configured user must be empty
(the default) or numeric `UID:GID`; this workload supplies no credentials override.
The workload prints a fixed marker and exits. Each measured attempt creates and
cleans up container resources in the supplied namespace and uses host networking.
Image resolution, pull/unpack, and lifecycle cleanup are inside the timer. A
prepared image does not make this a cache-only or offline benchmark; the adapter
still requests pull/unpack and may contact a registry. Environment preparation
and connect/probe are outside the timer. Use a dedicated benchmark environment.

```bash
cargo bench -p solti-benches --no-default-features --features containerd --bench container --locked -- 'container/containerd/'
```

The lane is skipped when the opt-in is absent and fails explicitly on missing
configuration or an unsupported host when opted in. A skipped lane is not a
measured result.

### Linux host policy

Set `SOLTI_BENCH_LINUX_HOST=1` and `SOLTI_BENCH_CGROUP_PARENT` to an explicitly
delegated, writable cgroup v2 parent with `pids` enabled in
`cgroup.subtree_control`. The benchmark creates per-attempt cgroups below this
parent and verifies their removal. It does not configure delegation itself.

```bash
cargo bench -p solti-benches --no-default-features --features host-policy --bench host_policy --locked
```

The child verifies its actual cgroup membership and pids limit. The seccomp
variant also reads back `NoNewPrivs` and seccomp mode. These cases exercise the
selected SDK controls; they do not certify a complete security boundary.
Non-Linux hosts skip this target.
