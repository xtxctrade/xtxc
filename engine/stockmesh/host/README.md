# Bounded Stocklana execution host

This is an implementation in progress, scoped to stock-token execution. No key
is stored or created by the sender. Public-network spending is not configured.

- `feed`: one bounded `getMultipleAccounts` response per immutable snapshot;
  slot monotonicity, content generation, missing-account and stale-state gates.
  Repeated replies from a frozen slot do not reset its freshness deadline.
- `market`: native NVDAx/USDC static Raydium CLMM proposals. Pool/config/mint/tick
  identities are bound. A quote still requires real instruction simulation.
- `quote-server`: loopback HTTP, two workers, a 64-connection queue, finite request
  and time budgets. `--fixture` measures quote ingress; `--live` uses confirmed
  mainnet account polling. Neither endpoint signs or settles a swap.
- `journal`: bounded append-only CRC/sequence log, exclusive ownership,
  fsync before acknowledgement, torn-tail recovery, fatal corruption, persistent
  duplicates and resource reservations. Full journals reject new work; checkpoint
  compaction and long-lived retention policy are not implemented yet.
- Signature and resource indexes avoid scanning every retained intent during
  admission. A signed transaction cannot be acknowledged under a second intent ID.
  Up to 64 journal admissions or phase changes share one durable group flush.
- `sender`: exact approved-message hash and every Ed25519 signature checked;
  uncertainty committed before RPC; same-wire retries; genesis pin; finalized
  result lookup before another send; unknown results retain reservations.
  A finalized success stays reserved until the caller verifies economic receipts.
  `step_batch` shares status/genesis/height reads across up to 64 intents and sends
  at most four transactions concurrently per sender. Uncertainty for the whole
  send group is durable before its first transmission. A group is not one atomic
  settlement: every signed transaction retains its own execution and receipt.
  Reservations belong to one journal owner; a production sharder must assign
  conflicting writable resources to the same admission authority. Separate journals
  do not create a distributed lock service.

`sender-step` is a trusted local proof/control driver, **not** an authenticated
public API. Its approval hash/resources must come from the trusted compiler and
user approval path. Production authentication and signer integration remain work.
Passing a caller-supplied hash through a public endpoint would not create approval.

All builds and tests run on AWS. `scripts/prove-host-load-aws.py` measures native
quote HTTP ingress, including overload rejection and recovery. It does not measure
settlement TPS. `scripts/prove-expanded-validator-aws.py --sender` exercises signed
SBF settlement through this sender, response loss and process restart, on a fresh
isolated validator with fixture funds and deleted ephemeral keys.
