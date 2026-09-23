# Monad order control plane (PR02)

This change adds a separate, private-file Monad journal and a loopback order
API. It does not deploy a contract, request a wallet signature, send a
transaction, or make the 112 observed stock tokens tradable.

The existing 1,074-instrument XTXC search directory and the 112 observed Monad
ERC-20 identities measure different things. The current Monad catalog has zero
admitted products and zero admitted venues. A stock becomes executable only
after its issuer product, rights, bidirectional venue route, quote and receipt
semantics have been reviewed.

Each journaled intent binds order ID, idempotency key, owner, chain ID, asset
ID, side, exact atomic quantities, quote digest and expiry. The journal syncs a
length- and CRC-framed event before acknowledging it, rejects duplicate
idempotency keys with changed intent and prevents one transaction hash from
being assigned to two orders. It rebuilds state after restart, truncates only
an incomplete tail and rejects complete corrupt frames. The process uses a
private regular file and a stable lock file; it cannot share the Solana sender
journal.

`SUBMITTED` means a wallet-reported hash, **not** a confirmed trade. Inclusion,
finalization and reorg transitions are separate, and the public API cannot
submit a chain observation. The loopback transport binds `127.0.0.1`, requires
a private gateway token, and has no signer or sender. Its prepare endpoint is
closed until a trusted quote-verification path is wired in PR03. Query/quote,
portfolio and ETF reads likewise report an unconnected dependency rather than
synthetic prices or balances.

The XTXC web consumer keeps the existing stock directory, adds an injected
Monad wallet boundary and a no-store API proxy. Wallet account/chain changes
invalidate prepared review; a transaction can only be requested from an exact
server-prepared payload. Web deployment and live customer trading are separate
later gates, not claims of this pull request.

Cherry-isolated test evidence: `cargo +1.94.0 test --manifest-path host/Cargo.toml
--target-dir /srv/skew/stockmesh-direct-node-20260920/build/monad-pr01-20260923
monad` passed 10 filtered tests including prior catalog tests; the dedicated
`monad-order-api` binary test passed 1/1. Web wallet tests passed 3/3 on Node
22.23.2. These are unit/fixture tests, not funded end-to-end execution.
