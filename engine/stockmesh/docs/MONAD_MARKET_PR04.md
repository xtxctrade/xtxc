# PR04 — public Monday RWA wallet route

Status: **IN PROGRESS**. No mainnet wallet signature, transaction submission, customer fill, or ETF mint was performed here. `/prepare` remains closed for the 112 observed products.

Monday's [public RWA launch notice](https://blog.monday.trade/rwas-are-live-on-monday-trade/) says a connected Web3 wallet can trade RWA markets without a Monday waitlist/KYC flow. We therefore do not use an issuer partnership as a prerequisite for implementing the public trading route. Issuer mint/redeem, market-making access, geographic eligibility, and ETF-vault transfer behavior are separate questions.

## Separate execution lanes

- `MONAD_ATOMIC`: the existing StockMesh executor and the direct Monday Spot V3 `aBIL/USDC` path. The pinned 112 × 5 fee-tier pool scan found only two direct USDC pools for `aBIL`; the 0.01% pool had zero active liquidity. This does not cover individual stocks.
- `ISSUER_ASYNC`: Monday RWA Router `0x2f903ac6ddaf57eadcbbc46adc3ad739c3506a2d` creates an order ID. Stock settlement occurs in a later transaction. Wallet stock or USDC delivery must be checked separately. No one-transaction ETF funding promise is made for this lane.

`host/src/monad/monday_public.rs` now encodes exact, wallet-direct buy/sell Router calls for any of the 112 observed token identities. The output is explicitly **unsimulated** and has no signing/submission switch. Input-token allowance, current venue price, proxy implementation, transaction simulation, cash/stock accounting units, order limits, and user consent must be checked before a real wallet call. In particular, the public market-order ABI does not contain a minimum stock-output argument.

`host/src/monad/monday_receipts.rs` now reads the finalized transaction body as well as its receipt, compares sender/target/calldata/value to the shown wallet proposal, and binds Router submission amount, stock and owner exactly. It still distinguishes submission from `Stock.MarketOrderSettled`; settlement alone is not wallet delivery. This EOA transaction-body check does not yet implement ERC-4337/smart-account attribution.

## Evidence and remaining gate

- Public Monday app bundle SHA-256 observed on Cherry: `ef1a7f43fb06158d9102fec38c7366ba05f13c6cd4985d1bb31e08bb8c08e22d`. Its Router and Stock ABI hashes are pinned in `host/src/monad/monday.rs`. The app bundle is observation, not a reviewed implementation source.
- Rust buy/sell ABI vectors were compared against `ethers.Interface` on Cherry. Bounded, read-only Monad finalized 32-block Router log probe returned zero matching orders; absence in that narrow window does not imply no public orders exist.
- Isolated Cherry `cargo +1.94.0 test --manifest-path host/Cargo.toml --lib monad::` passed **30/30** on 2026-09-24. Tests cover ABI bytes, bounds, direction, exact wallet-to-receipt binding, owner mismatch, finalized/canonical receipts, and prior order/journal behavior. Fixtures are not live fills.
- Current executable/admitted stock products remain **0**. Next proof is a public-route quote and same-state call simulation, followed by an explicitly owner-approved real USDC→stock→USDC order/settlement/wallet-return cycle. No fee collection or ETF source eligibility is inferred from this module.
