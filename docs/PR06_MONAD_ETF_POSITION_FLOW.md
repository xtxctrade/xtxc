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
about 598k/524k for cash/partial invest and 200k/437k for in-kind/cash exit.
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

## Monad Testnet receipt gate — accepted 2026-09-24

The demo ran on chain `10143` using only `DEMO_NO_EQUITY_RIGHTS` tokens. The
position-flow contract is `0xcaF944228e9597dfE31D4D16d3491F48A4933D0c`;
the PR05 vault is `0xcbaa5BdA0Ad0ce25f7ee836F99feb5738411B819`.
The independent chain-read verifier passed: 27 receipts, five economic events,
all expected nonces consumed, zero final ETF supply, zero vault reserves,
zero position-flow token balances, and issuance paused again. It checks the
0.5 bps fee against the actual event amounts, not just event presence.

| Action | Testnet transaction | Result |
| --- | --- | --- |
| Deploy position flow | `0x83d79bd7c3ebcc759a1cf11eff62d570d1c721192b1493d32e77824c7a68a4d4` | confirmed |
| Invest with demo USDC | `0xff4fa4bc752b05f3c939d415e5e22a5eebee3a1f4d13a5351c36848eebdfc3b9` | confirmed |
| Redeem half in kind | `0x22807f167f4d5ffb903a0f2de7791df3dcc6623d76dfbe2fe1e21d4b3536d9d5` | confirmed |
| Reinvest with wallet-held components | `0x62da672cda5210ca7dfaa4160564431aca3db2f08b96b3571418ac5267188805` | confirmed |
| Transfer shares and exit to cash | `0xf8787a4825348b95c46c2fec4e344e1f5deb8966be99bd57c1c1dafb93b12b2e` | confirmed |
| Redeem remaining shares to cash | `0xedc94237b2b000f45b7fe48f3994b79a44eccb924e034237e4364ab3de0c4d54` | confirmed |
| Pause issuance after demo | `0xe11e39a3a1d44fe35b1383e6201f2192e7fee188740f779e51a79ffdfc2611b9` | confirmed |

The first live run stopped after an RPC response error. The journal showed the
last approval confirmed, so `metropolis-etf-position-recover.cjs` checked all
prior receipts and resumed only missing actions. Two later transactions reverted
at insufficient explicit gas limits (`650,000` for partial invest and `600,000`
for the first cash exit). Both failed hashes remain in the evidence and the
verifier requires their failed receipts; their nonces were not consumed. The
retries used `1,100,000` and `900,000` gas limits respectively and succeeded.
The original demo script now uses testnet-specific gas headroom and funds its
test buyer before the wallet operations.

The full private evidence journal is retained on Cherry at
`/srv/skew/stockmesh-direct-node-20260920/runtime/pr06-etf/monad-testnet-position-flow-20260924.json`
(SHA-256 `1a7af95983f264aab63a4d8da5f199e295beda8c15e79d31d17053f8519f1060`).
Neither key file is included in the repository.

## Still outside PR06

- No real issuer rights, production stock-token admission or customer funds
  are established. Public-stock entry depends on confirmed issuer terms,
  executable buy/sell liquidity, and customer eligibility.
- Staged/async entry and exit are not executable from this PR alone; PR07
  supplies durable order recovery before any live use of those routes.
- The user-facing wallet review, portfolio and transaction UI remains in the
  private app's later UI PR, not in this public core repository.
