# XTXC basket vault — experimental fixed-unit v1

Not deployed. No selected mainnet program ID. Not a StockMesh program upgrade.
Legacy SPL only, 2–16 distinct assets. No customer funds or live-market acceptance.
Evidence: `[private verification path]`.

## Contract

One **share atom** claims immutable `unitsPerShareAtom[i]` token atoms of every
constituent. Configure share decimals and unit amounts together; selecting
dollar weights in a UI is NOT this integer composition. v1 cannot rebalance.
All token amounts are checked u64; multiply/add overflow rejects before CPI.

No deposit NAV oracle, floating NAV mint, administrator withdrawal, fees,
strategy trading, bonding curve, mint metadata or AMM/pool creation instruction.
Meme holders do not own these shares merely because a meme trades against them.
Creating a pair neither contributes reserves here nor changes the share claim.

Donations and direct SPL burns of shares leave **unallocated surplus**. They do
not change per-share redemption amounts. There is no surplus withdrawal method.
Do not value all vault reserves as redeemable holder equity. The frontend read
model is `stock-basket-unit-claims.ts`, not the floating-NAV prototype.

Stock issuers can retain freeze rights in constituent mints. Frozen or disabled
transfers stop the whole instruction. This program does not remove issuer rights,
provide an alternative exit, or promise cash redemption during a freeze.
The config's rights hash binds a definition; it is not proof of issuer consent.

## ABI

Authority: PDA seeds `b"xtxc-basket-v1", configPubkey, bump` under this program.
All supplied account identities must be distinct. Token program must be the
executable legacy SPL Token program. No Token-2022 extension admission exists.

### Initialize

Payload: `0:u8, constituentCount:u8, shareDecimals:u8, rightsHash:[u8;32],
unitsPerShareAtom:[u64 LE;n]`. n=2..16, decimals=0..9, positive unit amounts.
Exact payload length and nonzero rightsHash required.

Accounts in order:
1. New config, program-owned, writable signer, rent-exempt, zeroed, size112+72n.
2. Authority PDA, readonly.
3. Creator signer, readonly.
4. Preinitialized share mint, supply0, mint authority=PDA, no freeze authority.
5. SPL Token program.
6. Repeated (constituent mint readonly, empty vault writable).

Vaults must have authority=PDA and matching mint; no delegates, native balances
or close authorities. Reinitialization is rejected. Setup account creation and
mint/vault initialization must be composed by a separately reviewed factory.

Config layout: magic XTXCBV01[8], shareMint[32], creator[32], rightsHash[32],
bump:u8, n:u8, shareDecimals:u8, version1:u8, reserved0[4], then n records of
mint[32], vault[32], unitsPerShareAtom:u64LE.

### Mint / redeem

Payload: operation:u8 (1 mint, 2 redeem), shareAtoms:u64LE. Positive only.
Accounts:
1. Config readonly, owned by program.
2. Authority PDA readonly.
3. User signer readonly.
4. Share mint writable.
5. User share token account writable.
6. Fresh receipt, program-owned writable signer, rent-exempt zeroed128 bytes.
7. SPL Token program.
8. Repeated (constituent mint readonly, user's token account writable, vault writable).

Preflight proves every existing vault obligation is covered and every required
debit can be made. SPL TransferChecked moves actual constituents; observed
source/destination deltas must exactly equal the expected amount. Then real
MintToChecked/BurnChecked changes share supply and user holdings, again checked.
Any failure rolls back all of these changes atomically at the transaction level.

Receipt: magic XTXCBR01[8], config[32], owner[32], rightsHash[32], operation:u8,
reserved0[7], shares:u64LE, postSupply:u64LE. One committed receipt cannot repeat.
Wallet-signed receipt identity must stay fixed across UNKNOWN/retry. This is not
a substitute for the coordinator's durable launch/nonce journal. Generating a new
receipt after an unknown outcome could create a separately authorized operation.

## Verification

SBF SHA256: `33ed3da47a75cd037343b6ba3077d27b8ba63e7176dd71f84b7dd6ee0720b5f0`.
Archived SPL Token ELF SHA256:
`8190d3f7ceb6cb7a7a8d8924bff89f9f611e15ce1f806f2b6237f3311a98f697`.
Mollusk0.13.4 executes these binaries; mint/account/balance inputs are fixtures.
33 cases: 2/3/8/16-asset mint and redeem; partial/full; donation; reinit/replay;
unsigned user/receipt; foreign config/vault/recipient/receipt; delegated vault;
share freeze authority; frozen constituent; Token-2022; insolvency; alias;
zero/overflow; 10k and mid-CPI14k compute exhaustion with whole-account rollback.
3-asset mint19,477CU, redeem19,454CU; 16-asset mint76,677CU/redeem76,680CU.
CU success is not transaction packet/ALT or mainnet execution acceptance.

The `verification-only/*-keypair.json` file is deliberately **not a keypair**. It
prevents the verification build from silently generating a deployment credential.
Do not use it in deployment tooling. Tests use deterministic public keys only.

Remaining before release: eligible real mint/extension policy; program/factory
review; signed launch definition; durable funding and return coordinator;
unsigned wallet transaction assembly; packet/ALT setup; pool adapter;
share registry/indexing and actual portfolio/exit; release and capital approval.
