# PR07 — Monad order recovery (draft)

PR07 starts from PR02's single-writer journal and PR06's tested ETF position
flow. This change does **not** admit a new stock, deploy a mainnet contract,
send a transaction, or mark issuer submission as stock delivery.

## Implemented in this branch

- A journaled wallet order retains up to eight same-nonce transaction hashes.
  A replacement is classified as either the **exact previously prepared call**
  or a zero-value, empty-calldata wallet self-send. A browser-reported hash is
  checked against a provider-observed transaction before it is journaled.
- One owner and EVM wallet nonce can belong to only one economic order, even
  when two tabs report different transaction hashes. All hashes remain owned
  after compaction and restart.
- Read-only recovery checks chain ID, transaction body, receipt hash/status,
  removed logs, the finalized head, and canonical block hash. A missing
  receipt or an advanced wallet nonce without a registered winning hash stays
  `UNKNOWN`; it never authorizes an automatic retry.
- A finalized self-cancel or reverted execution is journaled as one terminal
  state. A successful Monday submission still requires its exact router event;
  transaction success alone cannot become stock delivery.
- The journal supports atomic, synced checkpoint replacement without dropping
  pending or terminal order identities. A torn last frame is recovered; a
  complete corrupt frame stops startup.
- `GET /v1/orders/{id}/resume` returns the original order and a stable action
  hint. `canAutoResubmit` is always `false`. The caller retains its original
  idempotency key and never creates a replacement economic intent from a
  timeout. `POST /v1/orders/{id}/report-replacement` binds a wallet-reported
  gas reprice or cancellation to the existing order after RPC verification.

The observer uses Monad's documented `eth_getTransactionReceipt`,
`eth_getTransactionByHash`, `eth_getBlockByNumber` `finalized` tag, and
`eth_getTransactionCount`. The [official JSON-RPC API reference](https://docs.monad.xyz/reference/json-rpc/api)
lists these interfaces; provider consistency is still checked per receipt.

## Cherry verification

Source and build were isolated under
`/srv/skew/stockmesh-direct-node-20260920/{source,build}/xtxc-monad-pr07*`.
The production StockMesh release, signer, funds, and mainnet submission
authority were not touched. Rust 1.91.1 was installed only under this build
directory because the system shell has no Rust toolchain. The focused Monad
host suite passed 48 tests and the order API suite passed five tests.

The integrated fake-provider checks cover a same-nonce gas reprice recovered
after journal restart, an actual finalized wallet cancellation, and a wallet
nonce consumed with no registered receipt. The checkpoint test proves hash
ownership and `UNKNOWN` survive compaction and restart. These tests exercise
the assembled local observer/journal but are **not** live transaction evidence.

## Required before PR07 acceptance

1. Connect an ETF-specific prepared intent and exact `ETFInvested` /
   `ETFRedeemed` event, share-transfer, cash/constituent delivery proof to the
   same durable recovery boundary. PR06's demo-only recovery script is not a
   customer order journal.
2. Add a bounded issuer settlement/wallet-delivery indexer and checkpoint.
   Monday `Finalized` here means the issuer accepted a submitted order, **not**
   that stock or cash reached the owner's wallet.
3. Wire the private web app to the resume contract, including wallet history
   review for the prepare-to-broadcast reporting gap. Do not claim an
   automatic safe retry when a wallet sent a transaction but the API never
   received its hash.
4. Run failure injection against a genuinely assembled, authorized route:
   pre-/post-broadcast stop, provider timeout, reprice/cancel, reorg, restart,
   and owner balance/fee/share accounting. No customer funds or mainnet
   authority are authorized by this PR.
