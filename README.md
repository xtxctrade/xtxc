# XTXC · StockMesh

**Tokenized-stock execution infrastructure for XTXC.**

[Live app](https://xtxc.trade/exchange?view=stocks) · [Architecture](docs/ARCHITECTURE.md) · [Review guide](docs/REVIEW.md) · [Run the tests](docs/VERIFY.md)

XTXC makes tokenized stocks approachable through stock discovery, wallet-owned
trading and portfolios. StockMesh is the execution layer underneath: it keeps
issuer and asset identities intact while compiling liquidity, allocating an
order, checking settlement and recovering its execution record.

This repository contains the engine, on-chain program source, interfaces and
tests. The consumer web application is available through the live link above;
its source and internal delivery plans are not part of this repository.

## Execution path

```text
User wallet / client
        │
        ▼
Product identity + policy admission
        │
Observed venue state → bounded curves → allocation
        │
Exact preparation → wallet authorization → execution
        │
Actual token deltas → settlement checks → durable receipt
```

## Inside the repository

| Component | Responsibility |
|---|---|
| [Compiler](engine/stockmesh/compiler) | Convert admitted venue state into bounded integer liquidity models |
| [Optimizer](engine/stockmesh/optimizer) | Allocate across eligible liquidity within explicit work and size bounds |
| [Reflow](engine/stockmesh/reflow) | Handle observed residual amounts without losing conservation constraints |
| [Adapters](engine/stockmesh/adapters) · [Native](engine/stockmesh/native) | Venue account decoding, native quoting and execution semantics |
| [Host](engine/stockmesh/host) | State, preparation, portfolio acquisition, order observation and recovery |
| [Settlement](engine/stockmesh/programs/stocklana-settle) | On-chain settlement constraints and token-delta checks |
| [Basket vault](engine/stockmesh/programs/xtxc-basket-vault) | Experimental fixed-unit basket deposit/mint and burn/return |
| [Tests](engine/stockmesh/tests) · [Examples guide](examples/README.md) | Reproducible core invariants and execution-interface examples |

Historical crate names such as `skew-engine` and `stocklana-settle` are preserved
to keep the implementation traceable.

## Run locally

The core is `no_std`, has no external dependencies, and does not use a network,
wallet, signer, clock or floating-point arithmetic.

```sh
cargo test --locked --manifest-path engine/stockmesh/Cargo.toml
```

Use Rust 1.85 or later for this core package. The exported core was tested with
Rust 1.94.0. Other Solana packages have separate toolchain requirements; see
[verification](docs/VERIFY.md).

## Review the implementation

- [Architecture and boundaries](docs/ARCHITECTURE.md)
- [Code-reading route and implementation scope](docs/REVIEW.md)
- [Setup and verification results](docs/VERIFY.md)
- [Source provenance](docs/PROVENANCE.md)
- [Security reporting](SECURITY.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

This is an import of existing source, not a new development history. Native
tests are not evidence of a mainnet fill; included programs are not represented
as independently audited or verified byte-for-byte against a live deployment.

Contact: [skewlabs@skew.deals](mailto:skewlabs@skew.deals)
