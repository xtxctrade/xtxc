# PR03 — Monad market state and trade-admission evidence

Status: **IN PROGRESS / NOT APPROVED FOR TRADING**. PR02 `/prepare` stays closed. This PR has no admitted BUY or SELL route and did not sign or submit a mainnet transaction.

## Existing observations

- The bounded, chain-ID-guarded scan observed bytecode for 112 Monday token candidates at one pinned Monad block. Code presence does not establish issuer rights, liquidity or executable XTXC orders. The 1,074-instrument frontend discovery catalog is separate; 83 tickers matched exactly and 29 Monad tickers require identity review.
- Monday's public app ABI exposes order-ID-returning `depositAndMarketBuy` and `depositStockAndMarketSell`, plus separate market-order settlement and internal stock-balance operations. The app bundle is an unreviewed observation, not an approved integration specification.
- `monadNewHeads` commitment updates, state gaps and reorganizations are handled fail-closed. The read adapter does not build or submit transactions.

## PR03 hardening on 2026-09-23

- Monday Router, Stock and Cashier are EIP-1967 proxies. The read probe now pins each proxy's implementation address and implementation bytecode hash at the same block. Pinning only the proxy bytecode would miss an upgrade.
- At Monad block 107287219, observed implementation addresses were Router `0x7809a84486d1fdcb2997503c08e1f0d019d51936`, Stock `0xd4b8d9a87bbb783e49ed0af91a0ea82a3e680b60`, and Cashier `0x4f98d2db6e25f09118177f41ad302d4b930cc31c`. This is a read-only snapshot, not a permanent allowlist. The isolated evidence file is `evidence/monad-pr03-20260923/monday-state-implementation.json`, SHA-256 `6183e9e080b5f099d09b195746db1688f5e20b51724a937221fd6a5b734c3962`.
- A new bounded receipt parser requires Monad chain ID, finalized inclusion, canonical block hash, successful status and unremoved same-transaction logs. It distinguishes Router order submission from matching Stock settlement and rejects mismatched owner, stock, direction, malformed amounts and duplicate settlement events. Even settlement is **not** by itself proof of token arrival in the user's wallet or USDC return.
- Focused isolated Cherry tests after the changes: `cargo +1.94.0 test --manifest-path host/Cargo.toml --lib monad::` — **18/18 passed**. These tests use fixtures for order events; they are not live order or fill evidence. Live production service, WAL, signer and funds were untouched.
- A bounded read of five major stock tokens against official Uniswap v3 Monad factory fees 100/500/3000/10000 found no pools for those pairs at the checked block. This excludes only those exact pairs/fees in that venue, not every possible route.

## Required before approval

1. Reviewed, current route-specific contract ABI/API, authorization and issuer/partner rights, with proxy-upgrade acceptance policy.
2. Same-state, time-bounded executable BUY and SELL quotes with input, output, fees and exact call/result comparison. A venue order-ID response is asynchronous submission, not atomic fill.
3. Real submitted-order, settlement and withdrawal receipts proving user-wallet stock delivery and USDC return, including failed, partial, expired and UNKNOWN recovery.
4. Two independently eligible stock constituents with actual wallet-to-ETF-vault deposit and return paths.

Until all four hold, `BUY=0`, `SELL=0`, `wallet delivery=0` and `ETF eligible=0` remain XTXC admission counts. Do not open `/prepare`, label a token tradeable, or approve PR03 for live trading from this evidence.
