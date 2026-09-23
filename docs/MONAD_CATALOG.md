# Monad stock catalog v1

This is the first interface for porting StockMesh's product identity to Monad.
It does not deploy a contract, authorize an adapter, or make any stock tradable.
The XTXC consumer web app remains outside this repository.

## Identity and admission

- `InstrumentId` groups the stock people search for; it is not an executable token.
- An executable product is pinned by Monad chain ID, ERC-20 address, issuer,
  issuer product ID, decimals and a rights-policy hash. Two tokens tracking the
  same company remain separate products.
- `tokenObservations` records 80 issuer-published Monad ERC-20 stock addresses
  and 18-decimal identity. These observations are not an executable allowlist.
- `venueObservations` records published contract addresses and observed
  execution class. Only an
  `admittedVenue` with a reviewed typed ABI and runtime-code hash may be named
  by an operation. The catalog declaration alone is not wallet or trade authority.
- A stock order path requires `BUY` and `SELL` evidence. Issuer orders may
  settle asynchronously; wallet stock withdrawal/deposit and cash movement
  are separate steps, not an instant-fill promise. A product may have the same
  operation at multiple venues. An ETF constituent additionally requires
  both vault deposit and withdrawal evidence.
- Arbitrary caller-provided target/calldata is not part of this interface.
  A later signed release, fresh state and exact simulation must still authorize
  any transaction.

`engine/stockmesh/monad/catalog/registry.v1.json` contains 80 official Anchored
stock token observations, 14 Monday launch candidates, Monad-native USDC,
the Anchored router/accounting contracts and two observed Monday contracts.
Its executable product and admitted venue lists are empty.
`deployment-manifest.v1.json` pins the catalog bytes and is `DISABLED`, with
no executor, ETF factory, vault implementation or approved fee policy.

Fee terms use decimal-string rational numerator/denominator. Thus 0.5 basis
points is `5/100000`; whole-bps truncation is not allowed. The value is an
arithmetic test vector, not an activated fee. ETF mint/redeem adds no second
execution fee to a constituent buy/sell. Composition encodes 2–16 exact
constituent product IDs and positive atomic quantities per share. The
synthetic test vector is labeled `fixtures` and grants no mainnet authority.

## Evidence boundary

Official sources identify [Monad mainnet as chain 143](https://docs.monad.xyz/developer-essentials/changelog),
[native USDC](https://developers.circle.com/stablecoins/usdc-contract-addresses),
[the issuer's 80 stock token addresses and transfer policy](https://docs.anchored.finance/getting-started/anchored-tokens),
[its Monad order/accounting contracts](https://docs.anchored.finance/trading-api/reference/environments-and-chains),
[Monday Trade contract addresses](https://github.com/monad-crypto/protocols/blob/main/mainnet/monday_trade.jsonc),
and [Monday's announced RWA markets](https://blog.monday.trade/rwas-are-live-on-monday-trade/).
The issuer's [order model](https://docs.anchored.finance/trading-api/getting-started/product-and-contracts)
says mUSD is a non-transferable accounting unit; a submitted transaction does
not prove an executed stock fill. Cherry read-only calls confirmed deployed code
and `decimals()=18` for aNVDA, aAAPL, aSPY, aGME and aRKLB. Those five form the
first integration cohort. Partner/API access, executed order receipts and
contract-vault behavior belong to later integration PRs. Issuer
[eligibility restrictions](https://docs.anchored.finance/getting-started/eligibility)
must be applied before exposing a live route to a user.

The importer `build_anchored_inventory.py` only rebuilds observations from a
saved official document and refuses catalogs with executable admissions.

Run the focused Rust tests on a build host:

```text
cargo test --manifest-path engine/stockmesh/host/Cargo.toml monad_contract --lib
```

The independent web parser consumes the same catalog and synthetic vector
files in its own tests. Neither parser test is evidence of a live purchase.
