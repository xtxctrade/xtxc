# PR04 Monday Spot read-only evidence (2026-09-23)

This is chain observation and isolated contract testing, **not** a mainnet
deployment, customer fill, issuer-rights approval, or PR04 live acceptance.

## Same-block cohort scan

Run `monad/scripts/scan-monday-spot.cjs` on the isolated Cherry workspace with
Node 22 and `ethers`. It makes at most 24 read-only JSON-RPC calls, checks
chain ID 143, batches `getPool(USDC, token, fee)` through Multicall3 and
rechecks the finalized block hash. No signer or submission API is used.

- Block: `0x666033b`, hash `0xb716af03b0cf030388df87f3b8f69e7e71eaf218268b16b63c6fe6d908a32857`.
- Inputs: all 112 `tokenObservations` in `registry.v1.json`; Monday Spot fee
  tiers `100, 300, 500, 3000, 10000`; all 560 `getPool` calls succeeded.
- Nonzero Monday direct USDC pools: aBIL fee 100 and fee 3000 only. This says
  nothing about other DEXes, routed pairs or Monday's separate issuer orders.
- Fee 100 pool `0x5bE19C2c1b698F7bBa7E899f17a4881267805a65`:
  active liquidity `0`, USDC balance `53` atoms, aBIL balance `52` atoms;
  both 1-USDC and 0.01-aBIL read-only quotes unavailable.
- Fee 3000 pool `0xb8700E0D0Df2B0b09A1374FbCdCC85E2E14F7898`:
  active liquidity `69905557358485131`, USDC balance `51153738840` atoms,
  aBIL balance `392654758755073459471` atoms. QuoterV2 reported
  1,000,000 USDC atoms → 10,881,624,879,181,574 aBIL atoms; 10,000,000,000,000,000
  aBIL atoms → 913,472 USDC atoms. These are small-size *quotes*, not fills.

The Monday SwapRouter at `0xFE951b693A2FE54BE5148614B109E316B567632F`
has runtime selector `0x414bf389` for the eight-field V3
`exactInputSingle` form. Its `factory()` call returned Monday's published
`0xC1e98D0A2a58fB8aBd10ccc30a58efff4080Aa21`. The contract source is not
verified on the checked public explorer or Sourcify, so a selector/factory
match is not a substitute for independent ABI/source review before deployment.

## Implemented in this PR

`MondaySpotVenue.sol` confines the existing `StockMeshExecutor` to one
USDC/token/pool/fee pair, checks the router/factory/pool code and pool pointer,
uses exact-input V3 swap semantics, resets approval and checks exact input
and output deltas. The contract is **not deployed** or listed as an admitted
venue. On Cherry, `npm test` passed the existing executor suite plus the new
adapter buy/sell, wrong caller/pool, partial spend, wrong recipient, router
failure and fee-on-transfer rejection cases. Ganache printed a µWS fallback
warning and continued successfully.

## Still required for PR04 acceptance

Verify a company-stock venue with executable buy/sell path, token transfer
rights for actual users, outer executor-call simulation against real Monad
state, separate deployment/security authorization, wallet approval and
signature, chain receipts for USDC → stock → USDC, and matching frontend
portfolio state. The only live Monday USDC pool found in this cohort is aBIL,
a tokenized treasury ETF, not an individual company stock. The 112 observed
tokens have not been promoted to executable products.

Sources: [Monday contract addresses](https://docs.monday.trade/spot-trading/spot-contract-pair-specifications),
[Monday pool fee tiers](https://docs.monday.trade/spot-trading/how-to-create-a-new-pool-on-monday-trade/create-a-new-pool),
[Monad JSON-RPC](https://docs.monad.xyz/reference/json-rpc/api).
