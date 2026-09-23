# Monad stock catalog v1

This is the first interface for porting StockMesh's product identity to Monad.
It does not deploy a contract, authorize an adapter, or make any stock tradable.
The XTXC consumer web app remains outside this repository.

## Identity and admission

- `InstrumentId` groups the stock people search for; it is not an executable token.
- An executable product is pinned by Monad chain ID, ERC-20 address, issuer,
  issuer product ID, decimals and a rights-policy hash. Two tokens tracking the
  same company remain separate products.
- `venueObservations` records published contract addresses. Only an
  `admittedVenue` with a reviewed typed ABI and runtime-code hash may be named
  by an operation. The catalog declaration alone is not wallet or trade authority.
- A stock buy path requires `BUY`, `SELL`, `DELIVER` and `RETURN` evidence.
  An ETF constituent also requires both vault deposit and withdrawal evidence.
- Arbitrary caller-provided target/calldata is not part of this interface.
  A later signed release, fresh state and exact simulation must still authorize
  any transaction.

`engine/stockmesh/monad/catalog/registry.v1.json` presently contains 14
issuer/venue *candidates*, Monad-native USDC and two observed Monday Trade
contracts. Its executable product and admitted venue lists are empty.
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
[Monday Trade contract addresses](https://github.com/monad-crypto/protocols/blob/main/mainnet/monday_trade.jsonc),
and [Monday's announced RWA markets](https://blog.monday.trade/rwas-are-live-on-monday-trade/).
Monday says its mUSD is a non-transferable accounting unit; it cannot be
substituted for USDC or assumed to be a composable ERC-20. The exact stock
token contracts, issuer terms, typed execution ABI, delivery/return receipts,
and vault-transfer rights remain to be established per product.

Run the focused Rust tests on a build host:

```text
cargo test --manifest-path engine/stockmesh/host/Cargo.toml monad_contract --lib
```

The independent web parser consumes the same catalog and synthetic vector
files in its own tests. Neither parser test is evidence of a live purchase.
