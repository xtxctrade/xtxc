# Architecture

The algorithm pipeline is described in [ALGORITHMS.md](ALGORITHMS.md). Read that
alongside this layer map: StockMesh has both off-chain planning modules and
on-chain economic enforcement, with different guarantees and resource limits.

## Identity before routing

The same stock can be represented by different issuer products, chains and
token mints. Grouping those products for discovery does not make their ownership
rights, eligibility, redeemability or execution paths interchangeable.

StockMesh carries product, issuer, mint and policy identity into preparation and
settlement. An available quote cannot override an admission constraint.

## Layers

| Layer | Source | Boundary |
|---|---|---|
| Integer execution core | `engine/stockmesh/lib.rs`, `compiler`, `optimizer`, `reflow` | No network, keys, clock, allocation or floats in the core |
| Runtime interface | `engine/stockmesh/runtime` | Explicit state snapshots, balances, execution host and resource admission |
| Venue implementation | `engine/stockmesh/adapters`, `native`, `quoters` | Native account layout, quote and instruction semantics |
| Execution host | `engine/stockmesh/host/src` | Provider state, exact preparation, order journal, portfolio acquisition and reconciliation |
| Settlement program | `engine/stockmesh/programs/stocklana-settle` | Validate identities, minima and actual token changes |
| Basket vault | `engine/stockmesh/programs/xtxc-basket-vault` | Experimental immutable token-unit claims; independent of trading-pool price |

## Planning and settlement share an economic target

The host admits product identity and coherent market state. OneBook expresses
compatible claim constraints; the compiler/solver turns eligible liquidity into
an allocation. Flow Folding can reduce compatible internal flows before external
execution. These components have distinct objectives, not one universal solver.

The signed envelope fixes identities, accounts, candidates and economic limits.
On chain, `claim.rs` and `exposure.rs` bind product conversions and verify the
aggregate share-exposure condition. `meshcell.rs` combines product-aware crossing
with residual graphs; `funding.rs` composes a common funding graph with that cell.
`allocation.rs` can refine residual allocation within the declared candidates.
`graph.rs` verifies each typed CPI's actual asset changes.

## State preparation and reuse

`host/src/stockmesh_api.rs` keeps independently refreshed banks and bounds the
hot set separately from catalog size. A quote publication binds its snapshot
revision, slot and content hash; stale or changed publication cannot silently
become an exact prepared transaction. Portfolio discovery and submission retain
their explicit provider/authorization boundaries.

`native/src/memo.rs` namespaces quote reuse by committed state and decoding
context. The on-chain oracle decodes stable tick/bin structures once and rereads
swap-mutated heads across legs. Only the small reference core is no-heap; native
models, the host and program adapters use separately bounded storage.

## Bounded allocation

The core compiles curves and allocates integer amounts under explicit bounds.
Its `WorkMeter` measures deterministic operation work, not Solana Compute Units.
Identity pins, asset domains and shared-liquidity constraints must survive
allocation and residual handling. Tests compare optimized paths with reference
calculations and exercise arithmetic, stale state and capacity failures.

## Durable execution

The host separates a quote, prepared transaction, submitted attempt and observed
settlement. A timeout does not turn an unknown attempt into a new economic order.
Journal and recovery paths retain the original attempt so that retries can be
reconciled with observed state.

Useful entry points:

- [Application API](../engine/stockmesh/host/src/stockmesh_api.rs)
- [Portfolio acquisition](../engine/stockmesh/host/src/investment.rs)
- [Execution journal](../engine/stockmesh/host/src/journal.rs)
- [Submission recovery tests](../engine/stockmesh/host/src/stockmesh_api/submission_recovery_tests.rs)
- [Portfolio discovery](../engine/stockmesh/host/src/stockmesh_api/portfolio_discovery.rs)

Client publication signatures and wallet transaction signatures have different
authority. Private keys and unrestricted spending permission are not inputs to
the credential-free review path.

## Basket claims are not market prices

The included basket vault is a fixed-unit, legacy-SPL implementation. Its share
represents specified in-kind constituent units, not an automatically rebalanced
dollar-weighted portfolio. It does not imply support for every token extension.

A meme token paired with a stock or basket share does not inherit the paired
asset's ownership or redemption rights. A trading pool, basket vault and wallet
holding are distinct objects. This repository does not claim a completed Meteora
DBC issuance/trading/migration integration.

## Operational separation

The client and provider configuration are outside the public source snapshot.
Price displays are not execution authority. Submission requires the relevant
user authorization; source availability grants none. Production secrets,
customer records, operating journals and deployment binaries are excluded.
