# solti

Thin façade over the modular Solti SDK.

`solti` contains no runtime logic. It forwards features to component crates and
exposes each crate through its canonical namespace:

```rust,ignore
use solti::core::SupervisorApi;
use solti::model::TaskSpec;
use solti::runner::RunnerRouter;
use solti::taskvisor::SupervisorConfig;
```

Default features are empty. Enable only the capabilities used by the binary:

```toml
[dependencies]
solti = { version = "0.0.3", features = [
    "api-core-adapter",
    "api-http",
    "core",
    "exec-subprocess",
] }
```

The `model` feature includes JSON Schema support from `solti-model`.

`exec-container` exposes the engine-neutral container runner.
`exec-containerd` adds the native containerd 2.x engine.
It does not add CRI or container network provisioning.

`full` enables every production component integration. Direct dependencies on
component crates remain supported.
