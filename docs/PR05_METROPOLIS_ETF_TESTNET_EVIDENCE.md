# PR05 — fixed-unit ETF vault: Monad Testnet evidence

Observed on Monad Testnet, chain ID `10143`, on 2026-09-24. This is a
**demonstration basket only**. Its dNVDA and dTSLA tokens confer no NVIDIA,
Tesla, fund, redemption-against-an-issuer, or other equity rights. The real
stock admission and legal product decision are separate.

## Contracts and definition

| Role | Address | Deployment transaction |
| --- | --- | --- |
| Second demo constituent, dTSLA | `0x9bB5CcB4C1ed8a29e167E0491AbBc92D204e866C` | [tx](https://testnet.monadscan.com/tx/0xa43a00a753d0ab4488573d2a063fcbe22b59ea69a3123eefd318604dc97ea315) |
| ETFFactory | `0x39C73fCb68618101837F8f6a2a992B36Cf15b985` | [tx](https://testnet.monadscan.com/tx/0x2cbb6d90708897110a0a777efc2ae6996f4a5a5483c9b46364b9890f7e8e8dea) |
| xdTECH ETFVaultShare | `0xcbaa5BdA0Ad0ce25f7ee836F99feb5738411B819` | [factory creation tx](https://testnet.monadscan.com/tx/0x730eec7bfbd6b0a715384ca0efc7dd00675df5702d7a640ba4d0dc9d2c85225c) |

The first constituent is the PR04 dNVDA at
`0x73225Ce4A4499E9a80720d5AEB1C4894f7f993e7`. The creator signed a
definition with two constituents, `100,000` token atoms of each per whole
share (`1e18` share atoms), a `1e13` share-atom granularity, version `1`, and
definition digest
`0xd5ecf1ae9165659b979d7a9965efd0e1a74344ab5bed19ac70a5a06e7837af2d`.
The factory admitted the two demo assets before creation; a different caller
cannot reuse that creator's signed transaction or nonce.

## Signed issuance, transfer and redemption

| Action | Observed outcome | Transaction |
| --- | --- | --- |
| Mint one share after exact constituent deposits | Confirmed; vault held `100,000` atoms of each asset | [tx](https://testnet.monadscan.com/tx/0x17d9d4e68f34f75d21db568c0cd43957a6bb35b23ca5995d9e3aac39d2d32351) |
| Transfer 40% of the share to another wallet | Confirmed | [tx](https://testnet.monadscan.com/tx/0x4fb0a647c91c34624aff2b65839fdee72f8b1f3ca28681b57a53ad6d74e00080) |
| Pause new issuance | Confirmed; mint static call rejected, existing redemption remained open | [tx](https://testnet.monadscan.com/tx/0x926287798b79e615bb8165ceb58b82be31984d927a04433cf12f242b3a38bc6e) |
| New holder redeems 40% | Confirmed; proportional assets returned | [tx](https://testnet.monadscan.com/tx/0x99d8bb33e855548f1d9eeccf39200e6cc29ad4f97ebd64d24f2c5976f48b9a61) |
| Creator redeems remaining 60% | Confirmed; total supply and both vault reserves returned to zero | [tx](https://testnet.monadscan.com/tx/0x3eb4ba73f0c3f0947c432480b6e6e77760e0d40787bcdaf061ab694e70ec2530) |

Measured testnet gas: mint `270,963`, transferred-holder redeem `207,618`,
remaining redeem `173,348`. The isolated chain journal and exact balance
assertions are retained on Cherry at
`/srv/skew/stockmesh-direct-node-20260920/runtime/pr05-etf/monad-testnet-etf-20260924.json`.
The test-only private keys are not part of this repository.

## Focused failure and recovery checks

On the isolated Cherry host, the existing Monad contract suite passed and the
new vault test covered 2, 3, 8 and 16 assets; exact issue/transfer/partial and
full redemption; unsupported share granularity; donated-asset surplus;
duplicate creator nonce and definition; simultaneous issuance by two wallets;
claim multiplication overflow; issue pause with redemption remaining
open; fee-on-transfer rejection; a later constituent transfer failure rolling
back prior legs and share burn; reentrancy rejection; and refusal to issue or
redeem from an insolvent reserve. The 16-asset local gas maximum was `960,544`
for issue and `678,532` for redemption. Rust host-side exact-claim tests (2/2)
and the private web arithmetic tests (5/5) passed on Cherry.

`PR05_TESTNET_ACCEPTED` means a functioning fixed-unit demo vault and its
in-kind round trip. It is **not** mainnet stock admission, public ETF sale,
USDC basket purchase or USDC exit; those are separate gates in PR06 onward.
