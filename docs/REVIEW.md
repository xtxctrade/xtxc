# Reviewer guide

## Product first, then implementation

Open [XTXC](https://xtxc.trade/exchange?view=stocks) to see the consumer stock
experience. The web source is maintained separately; this repository is the
engine and on-chain source submission. No funded transaction is necessary to
read the code or run the default tests.

## Code-reading route

| Question | Start here |
|---|---|
| What are the bounds and error conditions? | [Core](../engine/stockmesh/lib.rs) |
| How is liquidity represented and allocated? | [Compiler](../engine/stockmesh/compiler), [optimizer](../engine/stockmesh/optimizer) |
| How are residuals and conservation handled? | [Reflow](../engine/stockmesh/reflow), [kernel tests](../engine/stockmesh/tests/kernel.rs) |
| Where are native venue semantics? | [Adapters](../engine/stockmesh/adapters), [native implementation](../engine/stockmesh/native) |
| How are portfolio orders recovered? | [Investment](../engine/stockmesh/host/src/investment.rs), [journal](../engine/stockmesh/host/src/journal.rs) |
| Where are settlement checks? | [Settlement source](../engine/stockmesh/programs/stocklana-settle/src) |
| What backs a basket share? | [Experimental vault](../engine/stockmesh/programs/xtxc-basket-vault) |

## Scope of the evidence

The focused verification covers the native/core package. The host, adapters,
settlement program and basket vault are included for inspection, but their
presence does not establish production admission, an audit or deployed-byte
equivalence. Archived fixture results remain fixture results.

The basket vault is experimental and legacy-SPL-only. A completed mainnet DBC
launch lifecycle is not part of the verified source snapshot. Internal PR plans
and UI code are deliberately not published here.

## Submission materials

Link the real presentation and demo separately, along with actual
competition-period changes and the existing-work declaration. This source import
is not itself a hackathon submission or evidence that all code was written during
the event. See [provenance](PROVENANCE.md) and [verification](VERIFY.md).

The organization follows [Colosseum's repository guidance](https://colosseum.com/hackathon):
make the implementation and prior work easy to inspect. The
[Phoenix repository](https://github.com/Ellipsis-Labs/phoenix-v1) is a reference for
clear source/test/security organization, not an audit or endorsement of XTXC.
