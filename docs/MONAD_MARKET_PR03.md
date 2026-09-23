# PR03 — Monad market-state and venue-adapter evidence

Status: **IN PROGRESS**. This is read-only state and adapter research, not an executable XTXC stock route. The PR02 `/prepare` endpoint remains closed.

## Source and limits

- Work and builds: isolated Cherry path `/srv/skew/stockmesh-direct-node-20260920/source/stockmesh-monad-20260923/engine`. Live `/opt/stockmesh/current`, WAL, signer and funds were untouched.
- Monad JSON-RPC and WebSocket semantics: [Monad JSON-RPC overview](https://docs.monad.xyz/reference/json-rpc/overview). `latest` is proposed/speculative; `safe` and `finalized` have different commitments. `monadNewHeads` can deliver multiple commitment updates for the same block. `eth_getLogs` is range-limited.
- Official venue description: [Monday RWA launch](https://mondaytrade.beehiiv.com/p/rwas-go-live-in-public-launch). Monday describes USDC-funded buy and stock-funded sell with wallet delivery, but this description is not an XTXC quote, ABI or fill receipt.
- Read-only public Monday app bundle observed at `https://app.monday.trade/assets/index-1ofccodC.js`, SHA-256 `ef1a7f43fb06158d9102fec38c7366ba05f13c6cd4985d1bb31e08bb8c08e22d`. Its normalized router ABI SHA-256 is `d70a1e60cd5d0c9161e7f7facadf217a88687da3fba78fca93335ec2e8722a61`; stock ABI is `174a02b5cd8d35969f2bfc4a5310d59be756db68f5c93a90c989883b5ca053fa`. These are inspection fingerprints, **not reviewed/admitted execution ABI hashes**.

## Exact observations

- At Monad block `107252153`, hash `0x407a8089f973bc4789e563a7eeaa0b668998f9fb6ae8026c0a902ff4655f9c8d`, a chain-ID-guarded, 128-call-bounded read found deployed bytecode for all **112** catalogued token addresses. It re-read the same numbered block at the end and rejected hash drift. Calls used: **115**. The prior 32-code-hash cohort showed **0 mismatches**. Snapshot SHA-256: `c9edd8bf8a58fb8be1c577b312bb611596df7f282da780a3ec83b9c0b4057e67`. This proves code presence, not issuer rights or liquidity.
- `monadNewHeads` over the public WebSocket delivered **4** bounded notifications, including **2** finalized/verified updates. The tracker retained a contiguous head after handling older commitment updates. An earlier run correctly invalidated on treating a commitment update as a forward head; the parser was fixed and retested. Gap, reorg and unknown-fork logs fail closed in tests.
- At Monad block `107254969`, a read-only typed probe found bytecode for Monday's RWA router, stock and cashier; `router.stock()` and `router.cashier()` matched the public app configuration. Router/stock/cashier runtime SHA-256 respectively: `d409c74f9cc254da4d44f3c4b909fa5c686dbbe2745fcf63c5429a6cbab45c16`, `d4fe3bc08dc60309a933483db16a8ae9c329f611eba50d22d8cf789b0fdb658c`, `7b9b70f0a373fd1bb62562b47a3caf610ad2241061cca7fa444e2589c5e6e180`. The pinned result is `quoteAdmitted=false`; evidence JSON SHA-256 `bdd943a8af8435d748f0c01bf9c96552fca55835f2a8e673ffefc3da28508b8a`.
- The public router ABI contains `depositAndMarketBuy` and `depositStockAndMarketSell`, both returning order IDs. The stock ABI contains `settleMarketOrders` and `stockBalance`. Therefore order submission, internal stock balance, settlement and wallet delivery must be treated as distinct states. No one-transaction wallet delivery is inferred from a market-order call.
- The existing **1,074** display instruments were joined mechanically with **112** Monad token observations: **83** exact ticker matches and **29** unmatched Monad tickers retained for identity review. `BUY=0`, `SELL=0`, `wallet delivery=0`, `ETF eligible=0` are XTXC admission counts, not Monday volume claims.
- Cherry focused Monad tests: **14/14 passed**, including observed-only adapter rejection, out-of-order commitment updates, gap/reorg recovery, mixed-block scan rejection, unknown-fork log rejection and shared hot-set bounds.

## Remaining PR03 acceptance

1. A reviewed, current route-specific ABI/API and authorization model, including proxy implementation and upgrade tracking.
2. A time-bound real BUY and SELL quote with input/output/fees from one market state, followed by exact `eth_call`/execution-result comparison. Monday's order-ID path may be issuer asynchronous and must not be represented as atomic fill.
3. Actual order/settlement/withdrawal receipts showing stock-wallet delivery and USDC return, plus eligibility and issuer rights for XTXC integration.
4. Two independently eligible stock constituents whose wallet-to-ETF-vault deposit and return paths are demonstrated.

Until these hold, there is no admitted trade adapter and the XTXC executable catalog remains empty. No mainnet transaction was signed or sent in this PR.
