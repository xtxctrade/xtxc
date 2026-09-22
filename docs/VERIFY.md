# Setup and verification

## Credential-free core

Use Linux and Rust 1.85 or later for the root core package:

```sh
cargo test --locked --manifest-path engine/stockmesh/Cargo.toml
```

No RPC, wallet, provider account, environment file or private key is needed.
These tests cover bounded allocation, asset identity, arithmetic, conservation,
residual handling, admission and reference-model comparisons.

Target a test family while reading the implementation:

```sh
cargo test --locked --manifest-path engine/stockmesh/Cargo.toml --test kernel
cargo test --locked --manifest-path engine/stockmesh/Cargo.toml --test clearing
cargo test --locked --manifest-path engine/stockmesh/Cargo.toml --test optimizer_regressions
```

## Other packages

The host, native adapters, programs and SVM harness use separate Cargo manifests
and lockfiles. Building them can require additional dependencies or SBF tooling;
the root core test command does not build or verify them.

The basket proof harness also needs an independently supplied compatible SPL
Token ELF. Consult its README for the expected fixture identity. Production
artifacts and private replay captures are not distributed here.

Do not deploy a program or connect a funded wallet to reproduce unit tests.
Changing deployment authority is never a verification step.

## Snapshot checks

The source-only package is checked on an isolated Linux host. Exact results and
tool versions are recorded in [VERIFICATION.md](../VERIFICATION.md). A passing
build/test is not a measured execution advantage, live trade or independent audit.
