# PR04 — public Monday RWA wallet route

Status: **IN PROGRESS**. No mainnet wallet signature, transaction submission, customer fill, or ETF mint was performed here. `/prepare` remains closed for the 112 observed products.

## Metropolis testnet lane (separate from real stocks)

The hackathon path no longer waits on a mainnet funded USDC→stock→USDC round trip.
Official Monad Testnet is chain ID 10143, with public RPC and a faucet. The
isolated Cherry host received `0x279f` from `eth_chainId` and a current
`eth_blockNumber` from `https://testnet-rpc.monad.xyz` on 2026-09-24 KST.
The alternative `rpc.testnet.monad.xyz` shown in an older developer-portal
snapshot did not resolve from Cherry; use the verified testnet endpoint.

The executor now pins either Monad mainnet 143 or Testnet 10143 at deployment.
`MetropolisDemoAssets.sol` is restricted to Testnet 10143 and contains
explicitly non-equity demo cash/stock plus a constant-product demo venue. The
private Next.js route `/exchange/monad/testnet` keeps testnet token addresses
and wallet calls separate from Monday's 112 mainnet stock identities. It reads
two demo venue quotes, presents the better one, performs an exact allowance,
simulates the executor call, and asks the user's wallet to sign. A receipt
check refreshes token balances without treating a submitted hash as a fill.

Cherry isolated contract tests passed: existing atomic executor, Monday Spot,
and new Ganache-10143 demo buy→wallet stock→sell→wallet cash, faucet limit,
slippage rollback and nonce replay rejection. The private Next.js TypeScript
check and webpack production build passed; the new route appeared in the build.
These are **local EVM tests and a build**, not landed Monad Testnet transactions.

Hackathon acceptance requires actual Testnet deployment addresses and source,
test-wallet MON from the faucet, wallet-signed buy/sell transaction hashes,
decoded `Executed` events, holdings and cash after each transaction, and
refresh/recovery on the same order. The Testnet demo is not admission of a real
stock or evidence of Monday issuer settlement. Mainnet stock acceptance and
ETF product rights remain independent release gates.

Monday's [public RWA launch notice](https://blog.monday.trade/rwas-are-live-on-monday-trade/) says a connected Web3 wallet can trade RWA markets without a Monday waitlist/KYC flow. We therefore do not use an issuer partnership as a prerequisite for implementing the public trading route. Issuer mint/redeem, market-making access, geographic eligibility, and ETF-vault transfer behavior are separate questions.

## Separate execution lanes

- `MONAD_ATOMIC`: the existing StockMesh executor and the direct Monday Spot V3 `aBIL/USDC` path. The pinned 112 × 5 fee-tier pool scan found only two direct USDC pools for `aBIL`; the 0.01% pool had zero active liquidity. This does not cover individual stocks.
- `ISSUER_ASYNC`: Monday RWA Router `0x2f903ac6ddaf57eadcbbc46adc3ad739c3506a2d` creates an order ID. Stock settlement occurs in a later transaction. Wallet stock or USDC delivery must be checked separately. No one-transaction ETF funding promise is made for this lane.

`host/src/monad/monday_public.rs` now encodes exact, wallet-direct buy/sell Router calls for any of the 112 observed token identities. The output is explicitly **unsimulated** and has no signing/submission switch. For a buy, it conservatively requires deposited USDC atoms and requested mUSD atoms to be numerically equal; if Monday's unit conversion or fees make that false, this path stays blocked until the correct accounting and residual-withdrawal behavior are verified. Input-token allowance, current venue price, proxy implementation, transaction simulation, order limits, and user consent must also be checked before a real wallet call. In particular, the public market-order ABI does not contain a minimum stock-output argument.

`host/src/monad/monday_receipts.rs` now reads the finalized transaction body as well as its receipt, compares sender/target/calldata/value to the shown wallet proposal, and binds Router submission amount, stock and owner exactly. It still distinguishes submission from `Stock.MarketOrderSettled`; settlement alone is not wallet delivery. This EOA transaction-body check does not yet implement ERC-4337/smart-account attribution.

## Evidence and remaining gate

- Public Monday app bundle SHA-256 observed on Cherry: `ef1a7f43fb06158d9102fec38c7366ba05f13c6cd4985d1bb31e08bb8c08e22d`. Its Router and Stock ABI hashes are pinned in `host/src/monad/monday.rs`. The app bundle is observation, not a reviewed implementation source.
- Rust buy/sell ABI vectors were compared against `ethers.Interface` on Cherry. Bounded, read-only Monad finalized 32-block Router log probe returned zero matching orders; absence in that narrow window does not imply no public orders exist.
- Isolated Cherry `cargo +1.94.0 test --manifest-path host/Cargo.toml --lib monad::` passed **30/30** on 2026-09-24. Tests cover ABI bytes, bounds, direction, exact wallet-to-receipt binding, owner mismatch, finalized/canonical receipts, and prior order/journal behavior. Fixtures are not live fills.
- Current executable/admitted stock products remain **0**. Next proof is a public-route quote and same-state call simulation, followed by an explicitly owner-approved real USDC→stock→USDC order/settlement/wallet-return cycle. No fee collection or ETF source eligibility is inferred from this module.
