# SDK agent with Podium

Run a real SDK agent, deploy a task from Podium, and inspect its execution and
live output. Podium owns the saved specification and desired-state deployment;
the agent's core, runner, and Taskvisor own execution.

The agent declares its workload types: `exec.solti.io/v1` `Subprocess`, with a
JSON Schema of its body. It advertises the schema's fingerprint in every
heartbeat and sends the schema when Podium asks. Podium validates a spec's body
against that schema before it deploys it.

The demo runs this same executable in `--demo-task` mode. It writes 30 small
stdout ticks, writes start/finish messages to stderr, and exits successfully
after about 30 seconds. It uses no shell or external services.

## Start Podium

These commands assume sibling `podium/` and `sdk/` checkouts. Start both
terminals in their parent directory. Use the Go toolchain that Podium's `go.mod`
requires and the Rust toolchain required by the SDK workspace.

In the first terminal:

```sh
cd podium
GIT_OPTIONAL_LOCKS=0 GOMAXPROCS=2 go build -buildvcs=false -p=2 \
  -o /tmp/solti-podium-example ./cmd
export SOLTI_AUTH_JWT_SECRET="$(openssl rand -hex 32)"
/tmp/solti-podium-example \
  --config ../sdk/crates/solti/examples/podium/podium.yaml
```

Open <http://127.0.0.1:9080>. Sign in as `admin` with the password that Podium
logs once, at WARN with the prefix `ephemeral mode:`, at every start: the memory
backend keeps no data directory, so each start seeds a new administrator
password. The configuration contains no secret. `SOLTI_*` environment overrides
take precedence over the YAML; use the example values below for this local
instance.

| Boundary | Address |
|---|---|
| Podium UI and JSON API | `127.0.0.1:9080` |
| Podium HTTP discovery | `127.0.0.1:9082` |
| Podium gRPC discovery | `127.0.0.1:50061` |
| SDK agent HTTP Task API | `127.0.0.1:9085` |

This configuration uses `cluster.backend: memory`: Podium keeps its state in
memory and loses it, specifications and history included, when it stops. It binds
to loopback and uses plaintext, unauthenticated agent transport; it is a local
example, not a network deployment configuration.

## Start the SDK agent

In the second terminal:

```sh
cd sdk
cargo build --locked -j2 -p solti --example agent_podium \
  --features api-core-adapter,api-http,discover-http,exec-subprocess
export SOLTI_CONTROL_PLANE=http://127.0.0.1:9082
export SOLTI_AGENT_ADDR=127.0.0.1:9085
./target/debug/examples/agent_podium --print-podium-spec
./target/debug/examples/agent_podium
```

If `CARGO_TARGET_DIR` is set, use its example-binary path instead. The print mode
exits after producing JSON; the normal mode serves the agent until Ctrl-C.
It advertises `Podium example agent` with ID `podium-example-agent`, the
registered `default` subprocess runner with its `exec.solti.io/v1` `Subprocess`
type, and a discovery heartbeat every two seconds. In Podium, open **Agents** and
select this agent.

`--print-podium-spec` prints the exact Podium create-spec request, including the
absolute executable path and the schema fingerprint of the type the agent
declares. The command runs on the agent host, so keep that binary available while
its specification is deployed.

## The schema exchange

The heartbeat carries only the type's GVK and the schema's fingerprint, the
SHA-256 of the schema's canonical JSON. Podium answers with the fingerprints it
lacks, and the agent sends those schemas in follow-up requests of the same
attempt. A follow-up that fails is logged at WARN and does not fail the
heartbeat. Podium refuses a schema it cannot compile without telling the agent;
look for it in Podium's workload catalog, where it shows as `refused`, and in a
spec's placement detail, which reports `schema_incompatible`. If the type does
not appear in Podium's **Workload type** list after a few heartbeats, check the
agent's log for `discovery.schema_follow_up_failed`.

## Deploy from the browser

Open **Tasks → Add**. The builder is type-first: choose the workload type, then
one of the schemas Podium received for it, and write the body as JSON.

| Field | Value |
|---|---|
| Name | `SDK Podium demo` |
| Slot | `podium-example` |
| Workload type | `exec.solti.io/v1` `Subprocess` |
| Schema | The fingerprint of the agent's schema, the only one in this walkthrough |
| Body (JSON object) | The `workload.body` object from `--print-podium-spec`, with its absolute command path |
| Timeout (ms) | `60000` |
| Restart | `Never` |
| Target agent | `podium-example-agent` (the selector shows agent IDs) |

For the body, copy the printed `workload.body`. It has this shape, with the path
of your executable:

```json
{"mode": {"command": {"command": "/path/to/agent_podium", "args": ["--demo-task"]}}, "failOnNonZero": true}
```

Leave label selectors empty. Keep the remaining defaults. If the body
fails validation, Podium refuses it and lists the problems. Click
**Create Spec**, open the saved task, review its
placement preview, then click **Deploy** and confirm. Placement should select
this agent's `default` runner. Deployment is reconciled on Podium's next sync
cycle.

Open the agent page, find slot `podium-example`, and expand **Task details and
run history**. Open **Live logs** while the demo is still running. The drawer
shows arriving stdout/stderr; it does not replay output emitted before you
subscribed. After completion, inspect the terminal phase, run history, and exit
code. An exit code may be unreported even for a successful attempt; absence
alone does not indicate failure. A synced rollout confirms delivery of desired
state, not successful execution.

**History** can retain observed terminal attempts after Podium's collector sees
them. Collection is best effort; it is not a complete durable log of every run,
and it does not retain live output.

To run the demo again, edit its runtime timeout from `60000` to `61000`, save,
and deploy. This changes the desired runtime generation. Deploying an unchanged,
already-synced specification or only renaming it does not rerun the task.
Alternatively, create another demo specification with a different slot.

To save the exact JSON for an API client, run the print mode in a terminal
inside the SDK checkout:

```sh
PODIUM_SPEC="$(mktemp /tmp/solti-podium-spec.XXXXXX)"
./target/debug/examples/agent_podium --print-podium-spec > "$PODIUM_SPEC"
printf '%s\n' "$PODIUM_SPEC"
```

Use this JSON as the body for Podium's `POST /api/v1/specs` with an authenticated
session. Its `workload` object is `{api_version, kind, schema_fingerprint, body}`.
That call saves the spec and returns `204`; obtain its ID from
`GET /api/v1/specs`, then deploy with `POST /api/v1/specs/{id}/deploy`. The body
is a Podium request, not an SDK Task manifest. The agent's
`/apis/solti.io/v1/tasks` API is a separate contract: it takes
`exec.solti.io/v1` `Subprocess` as a routed workload, and rejects a body that
the agent's declared check refuses.

## Options and shutdown

| Environment variable | Default | Purpose |
|---|---|---|
| `SOLTI_CONTROL_PLANE` | `http://127.0.0.1:8082` | Podium HTTP discovery base URL, without an API path |
| `SOLTI_AGENT_ADDR` | `127.0.0.1:8085` | Agent listen address; port `0` selects a free port |
| `SOLTI_AGENT_ID` | `podium-example-agent` | Discovery identity and printed spec's explicit target |
| `SOLTI_AGENT_NAME` | `Podium example agent` | Name displayed in Podium |
| `SOLTI_AGENT_ADVERTISE_URL` | Bound listen address as an HTTP URL | Reachable agent base URL; required for a wildcard bind |

Apply the same custom `SOLTI_AGENT_ID` when printing the request and serving the
agent. URL settings are base URLs without credentials, query strings, or
fragments, not full API route URLs. Shared URL handling accepts HTTP(S) and reverse-proxy
base-path prefixes; HTTPS requires TLS configuration outside this plaintext
walkthrough.

For an already-running local Podium at UI port `8080` and HTTP discovery port
`8082` that accepts unauthenticated agent discovery, run `agent_podium` with its
defaults and open <http://127.0.0.1:8080>. This example sends no agent bearer
token. An instance requiring agent authentication needs an agent that supplies
its required credentials; the isolated loopback configuration is the matching
setup for this walkthrough.

When finished, keep the agent running, use Podium's **Delete** action for the
demo specification, and wait for reconciliation to remove its agent task.
Then stop the agent and Podium with Ctrl-C. Agent shutdown stops intake and
supervised tasks; Podium's memory state is gone with its process, so the next
start begins empty with a new administrator password.

## Troubleshooting

- **Agent missing:** use the discovery base URL `http://127.0.0.1:9082`, while
  `9080` serves the browser UI. Check the agent's discovery output for errors.
- **Address already in use:** choose unused ports in the YAML and matching agent
  environment variables. Leave unrelated services running.
- **Logs are empty after completion:** output is live-only. Open the drawer while
  the child runs; use a changed runtime generation for another execution.
- **No admin password in the log:** the password is logged at WARN with the
  prefix `ephemeral mode:` at each start. Search the log of the current run.
- **The workload type is missing or has no schema:** the schema arrives in a
  follow-up after the heartbeat. Wait a few heartbeats, then check the agent's
  log for `discovery.schema_follow_up_failed` and Podium's catalog for a
  `refused` fingerprint.
- **Podium refuses the body:** the body must match the agent's schema. Copy
  `workload.body` from `--print-podium-spec` and edit only the command path.
- **Old or missing demo executable:** rebuild the example with the listed
  features, check `CARGO_TARGET_DIR`, and print the request again. Update the
  saved command if its absolute executable path changed.
- **Wildcard bind cannot advertise itself:** set `SOLTI_AGENT_ADVERTISE_URL` to
  an agent base URL reachable from Podium. A wildcard listen address is not a
  destination for Podium's requests.
