# PR06 — Wallet-owned ETF entry and exit

This change composes the PR05 fixed-unit vault with admitted atomic stock
venues. The contract accepts a transaction only from the owner's wallet;
`receiver == owner`. It has no delegated signer, arbitrary target/calldata,
admin withdrawal, upgrade, or issuer-async path. All PR04/PR05 demonstration
tokens are **testnet-only** and confer no equity rights.

## Execution boundary

| User action | One-transaction path | Failure result |
| --- | --- | --- |
| Invest with USDC only | Pull bounded USDC; buy each required constituent; deposit exact vault units; mint ETF to owner; return cash/stock surplus | Entire transaction reverts; no shares |
| Invest with existing constituents | Pull only per-leg `fromWallet` atoms, buy the shortfall, then mint | No use of merely-approved wallet assets |
| Invest in kind | Pull the exact constituents, no venue or USDC debit | Entire transaction reverts |
| Redeem in kind | Pull and burn ETF share; send proportional constituents to owner | Entire transaction reverts |
| Redeem to USDC | Pull and burn ETF share; sell each constituent; send net USDC to owner | Entire transaction reverts; shares remain |

Buy fee is `floor(USDC spent / 20_000)` and cash-exit fee is
`floor(gross USDC proceeds / 20_000)`, both 0.5 bps. In-kind issue/redeem
charges no trading fee. MON gas is separate. Every transaction binds a wallet,
quote digest, deadline, unique nonce, share amount, max debit or net minimum,
and per-leg price floors. The router pins the registered PR05 vault definition,
the constituent code hashes, and each admitted venue code hash. It measures
actual token deltas, clears temporary allowances, returns only this order's
surplus, and retains no new customer balance after success. Prior donations
cannot satisfy the mint floor or be sent to the caller.

The Rust `monad/etf/{funding,prepare,exit}.rs` modules calculate fixed-unit
shortfalls, explicit wallet use, the budget-bounded share amount, platform fee
and refund. A route is marked atomic only when every leg is synchronous,
admitted, and within a measured gas ceiling. Issuer-asynchronous, cross-chain
or oversized routes are *staged*, with purchased assets remaining in the
user's wallet and mint readiness requiring independently observed component
balances. These modules do **not** submit or settle staged orders; PR07's
durable journal and receipt reconciliation are required for that live path.

## Isolated Cherry checks

`npm test` passed all five Monad contract suites. The PR06 suite covered
cash-only entry, partial holdings, in-kind-only entry, partial/full in-kind
and cash exit, external ETF transfer, fee/refund, stale price, insufficient
cash, wrong receiver, allowance, nonce replay, venue revocation, asset
admission revocation with existing redemption still available, and full
rollback on a later-leg failure. The two-component local gas samples were
about 596k/522k for cash/partial invest and 200k/437k for in-kind/cash exit.
The focused Rust ETF suite passed seven tests, including the share-budget
search and staged exit accounting. These are **local EVM tests**, not evidence
of a deployed product or live stock trading.

`scripts/metropolis-etf-position-flow.cjs` is a demo-only Monad Testnet
round trip using the PR04/PR05 tokens and vault. It journals each submitted
transaction before waiting. `scripts/metropolis-etf-position-verify.cjs`
independently re-reads receipts, five economic events, nonces, final vault
reserves and router balances. Deployment requires a separate explicit demo
broadcast marker and isolated testnet-only key files. No mainnet route is
admitted by these scripts.

## Remaining gates

- A testnet receipt/evidence run is required before calling PR06 testnet
  accepted. No local fixture or successful build substitutes for it.
- No real issuer rights, production stock-token admission or customer funds
  are established. Public-stock entry depends on confirmed issuer terms,
  executable buy/sell liquidity, and customer eligibility.
- Staged/async entry and exit are not executable from this PR alone; PR07
  supplies durable order recovery before any live use of those routes.
- The user-facing wallet review, portfolio and transaction UI remains in the
  private app's later UI PR, not in this public core repository.
