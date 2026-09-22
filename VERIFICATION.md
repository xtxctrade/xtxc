# Source snapshot verification

Prepared 2026-09-22. Checks run on an isolated Linux source-export checkout,
separate from the live service.

| Check | Result | Scope |
|---|---|---|
| Root core `cargo test --locked` | 35 passed, 0 failed | Native/core only; no SBF, provider or mainnet invocation |
| Manifest, links and excluded paths | Passed | 181 imported source files; 195 total tracked files; reviewer links resolve |
| Gitleaks 8.30.1 | Passed, no leaks found | Complete published Git history; precise public-literal exclusions |

Core toolchain: Rust 1.94.0. Source-only rerun completed on 2026-09-22.

The original mixed web/engine export was not published. This source-only
repository has a fresh import history so excluded web code and internal plans
are not recoverable from its Git history.

These checks do not verify the host or every included program, deployment-byte
equivalence, DBC mainnet integration, issuer access, performance superiority or
an independent audit.
