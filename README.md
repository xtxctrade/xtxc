# XTXC · StockMesh

**A stock-exposure aggregator, liquidity compiler, and on-chain execution kernel.**

[Live app](https://xtxc.trade/exchange?view=stocks) · [Algorithms](docs/ALGORITHMS.md) · [Architecture](docs/ARCHITECTURE.md) · [Review guide](docs/REVIEW.md)

XTXC gives users a stock trading experience. StockMesh compiles the fragmented
liquidity underneath it: different issuer tokens, AMMs, concentrated liquidity,
bin-based pools and order books.

The user's target is economic exposure to a stock. The engine selects eligible
issuer claims, allocates across executable liquidity and verifies what the wallet
actually received. Product mints remain distinct; a common underlying-share unit
is used for the aggregate execution condition, not to pretend different issuers
have identical rights.

This repository contains the engine, on-chain program source, interfaces and
tests. The consumer web application is available through the live link above;
its source and internal delivery plans are not part of this repository.

## The aggregator inside StockMesh

| Mechanism | What the code does | Read it |
|---|---|---|
| **OneBook + exposure settlement** | Express compatible issuer claims in underlying-share units, build state-bound virtual depth, and check aggregate conservative exposure on chain | [OneBook](engine/stockmesh/onebook/mod.rs) · [claims](engine/stockmesh/programs/stocklana-settle/src/claim.rs) · [exposure](engine/stockmesh/programs/stocklana-settle/src/exposure.rs) |
| **Marginal Liquidity IR** | Compile executable bands and constant-product models into bounded, non-increasing marginal curves with explicit approximation envelopes | [Compiler](engine/stockmesh/compiler/mod.rs) |
| **Global Marginal Solver** | Merge marginal liquidity, search admissible venue subsets under the leg limit, exact-evaluate finalists, and return an achieved output plus a model-bound gap | [Solver](engine/stockmesh/optimizer/mod.rs) |
| **Residual Tape** | Build the merged marginal ordering once; answer subset and residual-size queries through prefix rank/select without sorting again | [Tape](engine/stockmesh/optimizer/tape.rs) |
| **Native exact refinement** | Decode venue accounts into integer quote engines and refine token-atom allocations under quote-call, resource and leg budgets | [Native math](engine/stockmesh/native/src) · [refinement](engine/stockmesh/optimizer/oracle.rs) |
| **Flow Folding** | Clear compatible multi-asset flows through residual-graph circulation; reverse edges can undo earlier crossings and release better cycles | [Clearing](engine/stockmesh/clearing/mod.rs) · [FlowCell](engine/stockmesh/programs/stocklana-settle/src/flowcell.rs) |
| **On-chain residual allocation** | Re-read swap-mutated state, solve within the signed candidate set, execute typed CPIs and check actual token deltas | [Oracle](engine/stockmesh/programs/stocklana-settle/src/allocation.rs) · [graph](engine/stockmesh/programs/stocklana-settle/src/graph.rs) · [StockMesh cell](engine/stockmesh/programs/stocklana-settle/src/meshcell.rs) |

### 1. Aggregate the stock exposure, preserve the claim

An order for a stock can accept an explicit set of issuer products. StockMesh
keeps each exact mint and policy throughout execution, then evaluates the total
received exposure using conservative conversions. OneBook can cross compatible
claim constraints, while the settlement source enforces the corresponding
product-aware result.

**The optimization target is what the user receives across admissible claims,
not a ticker label or one pool's displayed price.**

### 2. Allocate at token-atom precision

The core solver uses marginal bands rather than a fixed menu of percentage
splits. Adaptive constant-product chords carry upper/lower error bounds. A
cardinality-constrained search ranks feasible subsets and exact-evaluates its
shortlist against the original admitted models.

`SearchReport` exposes `upper_output`, `achieved_output` and `gap_atoms`. The
upper bound applies to the admitted model envelope; it is not a claim to find
every possible route on Solana. Non-concave native-oracle refinement reports the
best observed feasible allocation rather than claiming global optimality.

### 3. Reuse computation while respecting state changes

Residual Tape preserves marginal order when candidates are removed. Its indexed
prefixes let the allocator reuse work for smaller residuals and different venue
subsets. A changed curve invalidates the tape.

Native CLMM/DLMM/Whirlpool quote code separates account decoding from repeated
integer quote evaluation. The on-chain oracle compiles tick/bin structures once
and refreshes mutable heads per leg. Worker quote memoization is keyed by a
namespace that must bind bank contents, edge order and decoder policy—not merely
a slot number.

These are concrete execution-cost optimizations in the source. Performance
against another router requires a matched benchmark; this repository does not
attach an unmeasured speedup to them.

## Off-chain planning, on-chain economic checks

```text
                         STOCK INTENT
                  cash budget / exposure floor
                              │
OFF CHAIN                     ▼
  Coherent instrument banks → issuer + policy admission
                              │
                 Native venue models / quote memo
                              │
           Marginal IR → Solver + Residual Tape
           OneBook / Flow Folding where applicable
                              │
              Typed graphs + declared candidates
                              │
                       USER SIGNATURE
                              │
ON CHAIN                      ▼
  Identity / nonce / policy / account / graph preflight
                              │
     Funding / signed crossing → residual allocation
                              │
           Typed CPI → observe actual balances
                              │
        Token minima + aggregate exposure checks
                              │
                    Commit / receipt
                              │
HOST                   Reconcile / holdings
```

The diagram maps the available source components, not a claim that every live
request runs every branch. An execution envelope contains predeclared accounts
and candidates; on-chain code does not discover arbitrary new pools. A failed
CPI is not treated as a successful zero fill. Multi-transaction capsules carry
sequencing and cumulative bounds, not cross-transaction atomicity.

### Hot-state architecture

The host keeps independently refreshed instrument banks. Catalog growth does
not expand hot polling concurrency one-for-one. Prepared snapshots and generation
checks separate a normal quote from final preparation, and per-worker quote
memos avoid recomputing the same amount against the same state.

Provider observations, exact simulation, user authorization and durable recovery
remain distinct. The current host includes explicit RPC-backed paths; a warm
memory quote is not a claim of RPC-free end-to-end execution.

## Bounded by design

The reference core uses **8 edges, 16 bands per curve, 4 execution legs and
3 reflows per envelope**. At 8 edges and a 4-leg cap, the solver considers at most
162 nonempty subsets and exact-evaluates up to 4 distinct finalists. These are
per-envelope computational bounds, not the number of listed stocks or a network
throughput claim. Native decoders and SBF programs have their own limits.

The dependency-free `no_std` core has caller-owned storage and checked integer
arithmetic. The host and native/SBF adapters use separate allocation strategies;
the no-heap property is not asserted for the entire system.

See [the algorithm walkthrough](docs/ALGORITHMS.md) for the objectives, reuse
mechanism, optimality boundaries and exact source/test mappings.

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

- [Algorithm walkthrough](docs/ALGORITHMS.md)
- [Architecture and boundaries](docs/ARCHITECTURE.md)
- [Monad stock catalog v1 (candidate stage)](docs/MONAD_CATALOG.md)
- [Code-reading route and implementation scope](docs/REVIEW.md)
- [Setup and verification results](docs/VERIFY.md)
- [Source provenance](docs/PROVENANCE.md)
- [Security reporting](SECURITY.md)
- [Third-party notices](THIRD_PARTY_NOTICES.md)

This is an import of existing source, not a new development history. Native
tests are not evidence of a mainnet fill; included programs are not represented
as independently audited or verified byte-for-byte against a live deployment.

Contact: [skewlabs@skew.deals](mailto:skewlabs@skew.deals)
