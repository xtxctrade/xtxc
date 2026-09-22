# Architecture

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
