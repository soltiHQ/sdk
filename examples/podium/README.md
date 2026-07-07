# `podium` — reference agent for a Podium control-plane

A single, config-driven Solti agent that connects to a [Podium](https://github.com/soltiHQ/podium)
control-plane end-to-end. One TOML file selects everything. Running without a reachable
control-plane is fine — the heartbeat just retries with backoff while the local API keeps serving.

> Podium itself is a separate project and is **not** built or run from here — this README
> shows the matching podium environment. That way the two halves line up.

## What this shows

- **transport** — HTTP or gRPC (runtime switch, no recompile);
- **TLS / mTLS** — on or off, with a shared CA;
- **bearer token** — on or off (same secret both directions);
- the agent exposes its own `TaskService` — podium can push specs to it;
- the agent heartbeats to podium's discovery endpoint and runs one local demo task.
  This gives you something to watch immediately.

## Layout

```
examples/podium/
├── config.toml         # quick start: HTTP, no TLS, no token
├── config.mtls.toml    # full setup: gRPC + mutual TLS + token
├── certs/gen.sh        # openssl: one shared CA + podium/agent (+ *-client) certs
└── src/{main,config}.rs
```

## Config reference

| Key | Meaning |
|-----|---------|
| `transport` | `"http"` or `"grpc"` — agent API **and** discovery transport |
| `agent.id` / `agent.name` | identity reported to podium |
| `agent.listen` | where the agent serves its TaskService (podium dials this) |
| `agent.advertise` | host:port advertised to podium as reachable |
| `control_plane.endpoint` | podium discovery endpoint (HTTP `:8082` / gRPC `:50051`) |
| `control_plane.heartbeat_ms` | heartbeat period |
| `[tls] enabled` | TLS for both the agent API (server) and discovery (client) |
| `[tls] ca` | shared CA: trust podium, and verify podium's client cert under mTLS |
| `[tls] cert` / `key` | agent server cert/key (podium → agent) |
| `[tls] mtls` | require a client cert on the API and present one to podium |
| `[tls] client_cert` / `client_key` | agent client cert/key presented to podium (mTLS) |
| `[auth] enabled` | shared bearer token on inbound API + outbound discovery |
| `[auth] token` / `token_file` | the token value, inline or from a file |
| `[task]` | optional local demo task (subprocess) |

Validation is strict: `tls.enabled` needs `ca`+`cert`+`key`; `tls.mtls` needs the client pair;
`auth.enabled` needs a token; unknown keys are rejected.

---

## Run

### Quick start — HTTP, no TLS, no token

**1. Start podium** (plaintext; from the podium repo):

```bash
cd /path/to/solti/podium
export SOLTI_AUTH_JWT_SECRET=$(openssl rand -base64 32)   # required, 32+ chars
go run ./...            # or: task ci/build && ./bin/podium
# HTTP discovery listens on :8082
```

**2. Start the agent** (from this directory):

```bash
cd examples/podium
cargo run -p podium                  # uses ./config.toml by default
```

You should see the agent log `http 0.0.0.0:8085 → heartbeat http://127.0.0.1:8082` and, within
`heartbeat_ms`, podium accepting the registration. The demo task prints the date every 5s.

Switch to gRPC by flipping `transport = "grpc"`, pointing `control_plane.endpoint` at
`http://127.0.0.1:50051`, and setting `agent.listen` / `advertise` to a gRPC port — still
plaintext, still works.

---

### Bearer token (no TLS)

Set `[auth].enabled = true` with a token, and start podium with `SOLTI_AGENT_AUTH_REQUIRE=true`.
Podium enrolls the token on the first sync (trust-on-first-use) and requires it thereafter — both
on inbound calls to the agent and on the agent's heartbeats. No certificates involved.

---

### TLS / mTLS — read this first

> [!IMPORTANT]
> **Today's podium applies server TLS *globally* to every listener** — the main UI (`:8080`),
> HTTP discovery (`:8082`) and gRPC discovery (`:50051`) all share one TLS config
> (`internal/app/app.go`). There is **no per-listener scoping**. As a result:
> - setting `SOLTI_TLS_SERVER_CLIENT_CA_FILE` forces **mTLS on the UI too** → browsers / health
>   checks get `tls: client didn't provide a certificate`;
> - podium's leader reverse-proxy picks the target scheme from the *incoming* request, not the
>   target listener (`internal/transport/http/middleware/leader.go`) → it dials `http://` into a
>   TLS port, flooding the log with `client sent an HTTP request to an HTTPS server` from `[::1]`.
>
> **Until podium gains per-listener TLS (or is patched), run podium plaintext** (the sections
> above). The `config.mtls.toml` preset and `certs/gen.sh` below are a correct reference for the
> **agent's** TLS/mTLS wiring — the agent builds and serves over mTLS fine — but pointing it at
> today's podium trips the global-TLS limitation above.

Generate the shared dev PKI (used by both sides, one CA):

```bash
cd examples/podium
./certs/gen.sh
# → certs/ca.crt  agent.crt/key  agent-client.crt/key  podium.crt/key  podium-client.crt/key
```

Run the agent with the mTLS preset (exercises the agent's full TLS path):

```bash
cd examples/podium
cargo run -p podium -- --config config.mtls.toml
```

Cert roles, for when podium can scope TLS to the agent endpoint:

| File | Used by |
|------|---------|
| `ca.crt` | shared trust root (both sides) |
| `agent.crt` / `agent.key` | agent's server cert (podium → agent) |
| `agent-client.crt` / `.key` | agent's client cert (agent → podium, mTLS) |
| `podium.crt` / `podium.key` | podium's server cert (agent → podium) |
| `podium-client.crt` / `.key` | podium's client cert (podium → agent, mTLS) |

The agent presents `agent-client.crt` to podium (verified against `ca.crt`), trusts podium via
`ca.crt`, and serves its gRPC API with `agent.crt` requiring podium's `podium-client.crt`. The
token is sent on every sync and required on every inbound call.

---

### Verify the connection

- **Agent logs**: registration accepted, heartbeats succeeding (no backoff retries).
- **Metrics**: `curl http://127.0.0.1:9090/metrics | grep solti_discover_` — `outcomes_total{outcome="success"}` climbing.
- **Push a task**: create a spec in podium (its UI/API) targeting this agent; podium `ApplyTask`s it
  to the agent's TaskService, and you'll see it run in the agent logs.

### Switching transport

Flip `transport` (`http` ⇄ `grpc`), point `control_plane.endpoint` at the matching podium port
(`:8082` HTTP / `:50051` gRPC), and adjust `agent.listen` / `advertise`. Nothing to recompile —
both transports are built in.

### Notes

- Metrics are always served on `:9090` (Prometheus).
- TLS and auth are independent toggles; any combination works.
- The generated `certs/*` are dev-only and git-ignored — never ship them.

## Next

| Example | What it adds |
|---------|--------------|
| [`agentd-http`](../agentd-http) | Env-driven agent, HTTP/JSON transport only |
| [`agentd-grpc`](../agentd-grpc) | Env-driven agent, gRPC transport only |
| [`tls-roundtrip`](../tls-roundtrip) | Minimal mTLS demo of `solti-tls` alone |
