---
title: Route work and add runners
description: Declare workload types, route work to the first matching runner, and implement an application-owned workload without changing the SDK model.
---

# Route work and add runners

A runner converts desired workload data into a reusable Taskvisor task.
It does not submit, schedule, or start that task while building it.
Use a custom runner when an application operation needs its own workload contract or backend.
A runner declares the workload types it runs: a GVK, the JSON Schema of its `spec`, and a check of the body.
The agent advertises those declarations to the control plane, which validates a spec against the schema before it deploys it.

## Participants

| Component | Role and reason to use it |
|---|---|
| `solti-model` | Validates the Task envelope, workload identity, labels, and selectors. A `Workload` carries application-owned JSON; `WorkloadDecl` declares its type. |
| `solti-runner` | Defines `Runner`, selects a backend, and passes environment, output, metrics, build cancellation, and build admission. It has no optional features. |
| `solti-core` | Commits desired state, builds the current generation, and submits its executable task. Use it when Tasks need reconciliation, queries, and lifecycle state. |
| Taskvisor | Executes attempts and owns runtime admission, timeout, restart, and cancellation after submission. |

Built-in backend implementations live in [solti-exec](subprocesses.md).
[solti-chain](chains.md) implements a composing runner through the same interface.
The model's `schema` feature derives workload schemas and describes wire data; it does not install a runner.

## Understand the integration flow

```text
TaskManifest → core desired-state commit → current Task generation
                                              ↓
                               exact workload GVK + runnerSelector
                                              ↓
                               Runner::build_task → BuiltTask
                                              ↓
                                  Taskvisor submission → attempts
```

`BuiltTask` pairs a `RunId` with a `taskvisor::TaskRef`.
Core uses the allocated run name for the runtime registration.
The model Task name, controller slot, and generated run name are different identities.
See [task resources](task-resources.md) and [lifecycle and admission](lifecycle-and-admission.md).

An accepted desired-state write does not prove that routing or building succeeded.
Read the Task's `Reconciled` condition for that generation.
A successfully built task can still be rejected by runtime admission.

## Select a backend deliberately

Routing is first-match: the task goes to the first runner, in registration order, whose exact GVK and labels match.
Podium evaluates that runner.
There is no schema-aware routing: the SDK never tries a second runner because the first one's schema or check refuses the body.

`RunnerRouter` applies these rules:

1. Reject built-in `Embedded` workloads. Their `TaskRef` is supplied through core's embedded-task API.
2. Keep the runners that declare the exact workload `apiVersion` and `kind`.
3. Apply `TaskSpec::runner_selector` to static runner labels, when configured.
4. Select the first remaining runner in registration order.

Several matches are allowed. The router emits a diagnostic and chooses the first; it does not return an ambiguity error or balance requests.
No match returns `RouterError::NoRunner`.
The router does not inspect application payload fields.

Two runners may declare one GVK, with different schemas.
Order them deliberately: only the first one, among those a task's selector admits, ever sees that GVK's tasks.

Register labels on the backend, not on the Task resource:

```rust
use std::sync::Arc;
use solti_model::Labels;
use solti_runner::{RouterError, Runner, RunnerRouter};

fn register_gpu(backend: Arc<dyn Runner>) -> Result<RunnerRouter, RouterError> {
    let mut labels = Labels::new();
    labels.insert("accelerator", "gpu");
    let mut router = RunnerRouter::new();
    router.register_with_labels(backend, labels)?;
    Ok(router)
}
```

Then use an object-valued selector in the Task spec:

```yaml
runnerSelector:
  matchLabels:
    accelerator: gpu
```

A Task's metadata labels serve resource selection and do not select its runner.

Registration validates and snapshots the runner name, labels, and declared workload types.
Names must be unique and valid label values; the workload list must be nonempty, contain no duplicates, and exclude built-in `Embedded`.
Changing an implementation's declarations later does not change the registered snapshot.
`capabilities()` returns that routing catalog as an owned capability snapshot.
Capabilities describe registration, not backend health or caller authorization.

## Declare workload types

A workload uses the normal envelope:

```json
{
  "apiVersion": "media.example.io/v1",
  "kind": "ImageResize",
  "spec": { "source": "input.png", "width": 320 }
}
```

The model validates the GVK, requires an object-valued `spec`, and bounds JSON nesting at 128 levels.
The envelope rejects unknown fields; the `spec` belongs to the application.
The `solti.io` API group is reserved for SDK-internal workloads; `exec.solti.io` and `chain.solti.io` are ordinary groups.

A runner returns its types from `Runner::workload_types`, as `WorkloadDecl` values.
Each declaration has a GVK, a schema, and a check that admits a body.
Three constructors cover the cases:

| Constructor | Schema | Check |
|---|---|---|
| `WorkloadDecl::of::<B>()` | Derived from `B`, which implements `WorkloadBody` and `schemars::JsonSchema` | Decodes the body into `B` and runs `B::validate` |
| `WorkloadDecl::new(api_version, kind, &schema, check)` | Hand-written JSON value | Your closure |
| `WorkloadDecl::object(api_version, kind)` | Any JSON object | Accepts everything |

`of` needs the model's `schema` feature, which a direct `solti-model` dependency enables by default.
Through the `solti` facade, enable `model-schema`.

```rust
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use solti_model::{WORKLOAD_SCHEMA_DIALECT, Workload, WorkloadBody, WorkloadDecl};
use solti_runner::RunnerError;

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImageResize {
    source: String,
    width: u32,
}

impl WorkloadBody for ImageResize {
    const API_VERSION: &'static str = "media.example.io/v1";
    const KIND: &'static str = "ImageResize";

    fn validate(&self) -> Result<(), String> {
        if self.source.trim().is_empty() || self.width == 0 {
            return Err("source and positive width are required".into());
        }
        Ok(())
    }
}

fn decode_spec(workload: &Workload) -> Result<ImageResize, RunnerError> {
    workload
        .decode::<ImageResize>()
        .map_err(|error| RunnerError::InvalidSpec(error.detail()))
}

fn declarations() -> Vec<WorkloadDecl> {
    let probe_schema = json!({
        "$schema": WORKLOAD_SCHEMA_DIALECT,
        "type": "object",
        "required": ["source"],
        "properties": { "source": { "type": "string", "minLength": 1 } },
        "additionalProperties": false,
    });
    vec![
        WorkloadDecl::of::<ImageResize>(),
        WorkloadDecl::new(
            "media.example.io/v1",
            "ImageProbe",
            &probe_schema,
            |workload: &Workload| match workload.body().get("source").and_then(|v| v.as_str()) {
                Some(source) if !source.trim().is_empty() => Ok(()),
                _ => Err("spec.source: a non-empty string is required".into()),
            },
        ),
        WorkloadDecl::object("media.example.io/v1", "Thumbnail"),
        WorkloadDecl::object("media.example.io/v1", "Poster"),
    ]
}
```

A declaration never fails its constructor.
A refused GVK, a reserved group, or a schema outside the supported subset is kept and surfaces as `RouterError::InvalidWorkloadType` when the runner is registered.
The router reads `workload_types()` once, at registration, and snapshots it.

The check must be fast and non-blocking.
Core runs it synchronously, under the Task's write lock and a state-write admission permit, on every create and apply of a task of that type.
A check that waits stalls every write to that Task and holds a permit other writes need.
Decode and compare; do not do I/O and do not look anything up.
That includes `WorkloadBody::validate`, which a derived declaration runs inside its check.

### Several types, several runners

One runner declares up to 256 types, with distinct GVKs.
An agent registers up to 64 runners and advertises up to 256 distinct schema fingerprints in all.
The runner above declares four types.
`Thumbnail` and `Poster` have the same schema, so they share one fingerprint: the agent sends that schema once and Podium stores it once.
`AgentCapabilities::schemas` lists each distinct schema once, in first-declaration order, and `types_for` lists the GVKs that share one.

Registration order across runners is routing priority and the order in which the agent advertises them.

### What the schema is

The schema describes the `spec` alone.
Its identity is the fingerprint: the lowercase hex SHA-256 of the schema's RFC 8785 canonical JSON (sorted keys by UTF-16 code unit, ECMAScript number formatting, minimal string escapes).
The agent sends exactly those canonical bytes when Podium asks for the schema, so a fingerprint always names the bytes that hash to it.
`WorkloadSchema::canonical_json` returns them and `fingerprint` returns the hash:

```rust
use solti_model::WorkloadDecl;

fn inspect() {
    let decl = WorkloadDecl::object("media.example.io/v1", "Thumbnail");
    let declared = decl.workload_type().expect("a usable declaration");
    assert_eq!(
        declared.schema().canonical_json(),
        br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
    );
    assert_eq!(declared.fingerprint().as_str().len(), 64);
}

inspect();
```

Everything in the canonical bytes is part of the contract, not only the validation keywords:

- **The root `title` is the Rust type's name.** schemars writes it, and so it writes the names of nested types as the keys of `$defs`. Renaming a body type, or a type nested in it, changes the fingerprint although no body changes. Podium sees a new schema. Pin a name with `#[schemars(rename = "...")]`.
- **Doc comments become `description`s,** so editing one changes the fingerprint too.
- **`$schema`** is draft 2020-12 or absent.

Treat any change to the schema, wanted or not, as a new fingerprint.

### Schema rules

`WorkloadSchema::from_value`, which every declaration runs, refuses at registration everything Podium's compiler refuses.
Podium refuses a schema it dislikes without telling the agent, so the SDK check is the only warning you get; see [what the agent can see](discovery.md#exchange-workload-schemas).

**The subset.** Draft 2020-12 with its core, applicator, validation, annotation and content keywords (52 in all, listed in the [subset source](../crates/solti-model/src/domain/workload/schema/subset.rs)), with these exceptions:

- refused: `$dynamicRef`, `$dynamicAnchor`, `$recursiveRef`, `$recursiveAnchor`, `$vocabulary`, `unevaluatedProperties`, `unevaluatedItems`;
- `$ref` resolves only inside the document: `#`, `#/json/pointer` or `#anchor`; a remote or unresolved reference, a duplicated `$anchor`, an `$id` below the root and a cycle of subschemas at the same location of a body are refused;
- unknown keywords and `x-*` keywords are ignored.

**The limits.**

| Limit | Value |
|---|---|
| Canonical JSON | 65 536 bytes |
| Nesting of objects and arrays | 32 levels, the root object is level 1 |
| Objects and booleans, wherever they are | 512 |
| One regex | 1 024 bytes of source; 2 000 instructions, a class counted by its ranges |
| All regexes of one schema | 16 KiB of source; 20 000 instructions |
| `\p` or `\P` written in one regex | 15 |
| `\p` or `\P` written in one schema | 153 |

The last two are Podium's regex budget: it charges each `\p` or `\P` in the raw text 833 units of parse work, against 128 000 for a schema, and 153 × 833 fits where 154 does not.
Every `\p` counts, even the `\p` of an escaped backslash, as in `\\p`.
A rejected schema names its location, such as `#/properties/name`, and the rule it breaks.

**Regexes.** Podium compiles each `pattern` and `patternProperties` key with Go's RE2 parser; the SDK reads it with Rust's `regex-syntax` and refuses a pattern unless both read it alike.
Some rules are therefore stricter in the SDK than in Podium, and a pattern Podium would accept can still be refused here:

- `\p{...}` and `\pL` classes: Podium sizes them from Go's Unicode tables, which the SDK cannot reproduce. Write explicit ranges.
- Syntax that Rust and Go read differently: `\<`, `\>` and `\b{start}`-style boundaries; `&&`, `--` and `~~` inside brackets; a nested `[` or a leading `--` in a class; a counted repetition with a space or a leading zero, as in `a{ 2}` or `a{01}`.
- Go-only syntax that `regex-syntax` cannot parse: `\Q...\E`, octal escapes such as `\0`, a `{` that starts no repetition such as `x{,5}`, `(?)`, and class escapes before `-` as in `[\d-z]`.
  Go may accept them, but the SDK cannot parse them to check them, so it refuses them (`SdkStrictRegexRule::GoOnlySyntax`).

Both sides refuse look-around, back-references, `\u`, `\Z` and counts over 1 000.
Near the instruction limits the SDK may also refuse a pattern Podium accepts: it counts a class of two non-ASCII fold-equivalent characters, such as `[Δδ]`, as a class, where Go rewrites it as a case-folded literal, so the SDK's count is higher. Write the literal form instead.
Case-insensitive matching is not available: Podium refuses `(?` followed by `i`, `m`, `s`, `U` or `-` anywhere in a pattern's text, whether or not it is a flag group.
That also refuses a pattern that only wants an optional parenthesis, such as `^\(?-?\d+\)?$`, because it contains `(?-`.
Write the optional parentheses as classes instead:

```json
{ "type": "string", "pattern": "^[(]?-?\\d+[)]?$" }
```

**Integers.** JSON peers read integers exactly only up to 2^53.
Integers in a body, nested ones included, must lie within plus or minus 9 007 199 254 740 991 (`MAX_JSON_SAFE_INTEGER`).
A derived schema therefore bounds every `i64`, `u64`, `usize`, `isize`, `i128` and `u128` field to that range, keeping a tighter bound you set, so that Podium refuses a larger value when the spec is created.
A hand-written schema must bound its large integers itself.

**`flatten`.** Avoid `#[serde(flatten)]` combined with `#[serde(deny_unknown_fields)]`.
serde does not support the pair, so the derived schema and the decoder can disagree about the fields they accept.
The SDK rewrites the `unevaluatedProperties` that a flattened enum makes schemars emit, so Podium still accepts the schema; the disagreement is yours to avoid.

### Admission

Core admits a routed task before it stores or builds anything.
It picks the runner exactly as the router does, the first match of GVK and selector, and runs that runner's check on the body.
The runner that checks a body is the runner that would build it.
With two runners for one GVK, the first runner's check decides unless the task's selector picks the second.

Admission refuses with `CoreError::InvalidSpec`, which the Task API maps to HTTP 400 and gRPC `InvalidArgument`:

| Case | Message |
|---|---|
| No runner declares the GVK, or none matches the selector | `no runner matches <apiVersion>/<kind>` |
| The first matching runner's check refuses the body | `workload <apiVersion>/<kind> rejected by runner '<name>': spec.<path>: <reason>` |
| An `Embedded` workload on a routed write, or a routed one on an embedded write | `spec.workload kind Embedded requires create_embedded_task() or apply_embedded_task()`, or `a caller-supplied TaskRef requires spec.workload kind Embedded` |

`spec.<path>` names the offending field, below `spec`; a bad value at the root reads `spec: <reason>`.
Nothing is committed when admission refuses.

Podium re-checks compatibility before every push.
A target whose schema no longer accepts the body gets no push, and the rollout is marked failed.
The agent needs no code for this; read the failure in Podium's rollout and placement detail.

### Built-in types

| GVK | Crate and feature | Body |
|---|---|---|
| `exec.solti.io/v1` `Subprocess` | `solti-exec`, `subprocess` | `SubprocessSpec`: `mode` (`command` or `script`), `env`, `cwd`, `failOnNonZero`; unknown fields refused |
| `exec.solti.io/v1` `Container` | `solti-exec`, `container` | `ContainerSpec`: `image`, `command`, `args`, `env`; unknown fields refused |
| `chain.solti.io/v1alpha1` `Chain` | `solti-chain` | `ChainSpec`: `entry` and `steps`; each nested step workload is checked at admission under the step's selector |
| `solti.io/v1` `Embedded` | `solti-model` | In-process `TaskRef` revision; never routed, never declared, never on the wire |

`Embedded` carries a prebuilt `TaskRef` that the binary supplies through core's embedded-task API.
It bypasses routing and capabilities, the Task API refuses it as input and the adapter hides it from reads.

There is no `Wasm` type.
It was a model type and a proto message that never had a runner in this repository, and Podium treats no type specially.
An application that embeds a WASM engine declares its own type, for example `wasm.example.io/v1` `Module`, with its own schema and runner.

### Float parsing

Both canonical-JSON crates turn on `serde_json/float_roundtrip`, and Cargo feature unification applies it to every `serde_json` user in the build.
Floating-point numbers are then parsed correctly rounded, slightly slower than the default.

## Validate the payload in the runner too

The check admits a body before it is stored.
`build_task` still decodes it, as `decode_spec` does in the first example. Core builds only tasks that passed admission, but `RunnerRouter::build` called directly runs no check, and a persisted task can predate a stricter schema. Handle the types you declared and refuse the rest.

The [custom workload example](../crates/solti-runner/examples/custom_workload.rs) uses this pattern.
Use `RunnerError::UnsupportedWorkload` for the wrong workload and `InvalidSpec` for invalid desired data.

## Keep build and attempt ownership separate

Implement `Runner` with `#[solti_runner::async_trait]` and three methods:

- `name()` identifies the backend registration;
- `workload_types()` declares the supported workload types, with their schemas and checks;
- async `build_task(task, run_id, context, cancellation, scope)` returns a `TaskRef`.

Build work must remain owned by the build future.
Observe `BuildCancellation` during interruptible waits and pass it to owned child operations.
Do not detach blocking or asynchronous build work.
An inherently blocking backend needs its own bounded facility and explicit shutdown contract.

The returned task must not retain the build cancellation signal.
Each attempt receives a separate `TaskContext`; use that for runtime cancellation.
Create attempt-scoped processes, connections, and cleanup state inside the returned future.
One `TaskRef` may run more than once under restart policy, including after an earlier attempt failed.
See [cancellation and shutdown](cancellation-and-shutdown.md).

Construction failures are `RunnerError`/`RouterError`, not failed execution attempts.
Runtime work returns `TaskError`: distinguish retryable failure, fatal failure, and cancellation.
The selected runtime policy decides whether a retryable result starts another attempt.
Do not encode retry decisions by matching error text.

## Bound managed builds

Core's `ReconciliationConfig` defaults to a 30-second build deadline, 32 outer builds, and 8 builds per runner.
The deadline begins before root admission and includes nested builds and their admission waits.
It is separate from the Task's attempt timeout and controller queue residence.

Direct `RunnerRouter::build` and `RunnerCatalog::build` are unmanaged.
They do not inherit core's build limits or deadline.
Direct callers can use `BuildCancellation::pair()` and `build_with_cancellation`; the caller retains the owner handle and supplies its own deadline.

A composing runner must pass the received `BuildScope` into `RunnerCatalog::build_scoped_with_cancellation`.
Nested builds reuse the outer global permit and acquire the selected runner's per-runner permit.
Do not replace the scope with an unmanaged one.
Re-entering an active runner returns `RecursiveBuild`; a nested wait that closes an admission deadlock returns `AdmissionCycle`.
Preserve nested errors with `RunnerError::NestedBuild` when composing backends.

See [reconciliation](reconciliation.md) and [configuration](configuration.md) for the application-owned settings.

## Pass environment and output through the context

`BuildContext` defaults to an empty `RunnerEnv`, no-op metrics, and disabled output.
`merge_env` keeps the last value within each input and lets runner values override task values.
Concrete backends decide how that result combines with a host or image environment.

Acquire `OutputPublisher::sink_for` from the attempt future before spawning output-reader tasks.
Clone the returned sink into readers.
Composing runners can use task-local output routing that a separately spawned task does not inherit.
The sink carries the attempt and generation; stdout and stderr have independent sequences.
Publishing is synchronous, and callbacks must not block execution.
Runners publish chunks, not lifecycle markers or retained history.

Core installs its output publisher when it consumes the router.
For direct builds, install your own publisher if output is required.
Installed metrics and output callbacks have sticky panic containment; callback failure is not a durable delivery guarantee.
See [output and history](output-and-history.md) and [observability](observability.md).

## Run an existing example

```bash
cargo run -p solti-runner --example custom_workload
cargo run -p solti-runner --example build_context
```

`custom_workload` registers two backends for one GVK and selects one by label.
Its task prints the chosen operation; it does not implement image processing.
`build_context` demonstrates environment precedence, metrics, and attempt-scoped output.
For a complete supervised execution path, use [task_subprocess.rs](../crates/solti/examples/task_subprocess.rs).

## See also

- [Build an agent](building-an-agent.md): own startup, services, and shutdown.
- [Subprocesses](subprocesses.md), [containers and isolation](containers-and-isolation.md), and [chains](chains.md): concrete runner processes.
- Source: [runner contract](../crates/solti-runner/src/runner.rs), [routing](../crates/solti-runner/src/router.rs), [build admission](../crates/solti-runner/src/admission.rs), [build context](../crates/solti-runner/src/context.rs), and [workload declarations](../crates/solti-model/src/domain/workload/decl.rs), [workload schemas](../crates/solti-model/src/domain/workload/schema/mod.rs).
