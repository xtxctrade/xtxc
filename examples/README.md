# Execution examples

The existing test cases are runnable, self-contained examples of the core API.
They use synthetic inputs deliberately; they do not represent live market quotes.

| Example | Read | Run |
|---|---|---|
| Compile bounded curves, allocate, then handle residuals | [Kernel](../engine/stockmesh/tests/kernel.rs) | `cargo test --locked --manifest-path engine/stockmesh/Cargo.toml --test kernel` |
| Check clearing and conservation | [Clearing](../engine/stockmesh/tests/clearing.rs) | `cargo test --locked --manifest-path engine/stockmesh/Cargo.toml --test clearing` |
| Compare allocation against reference behavior | [Optimizer regressions](../engine/stockmesh/tests/optimizer_regressions.rs) | `cargo test --locked --manifest-path engine/stockmesh/Cargo.toml --test optimizer_regressions` |

An execution host implements the interface in
[runtime](../engine/stockmesh/runtime/mod.rs). The fixture host exercises that
interface without a network, signer or chain submission. The production-facing
host is a separate component under `engine/stockmesh/host`.
