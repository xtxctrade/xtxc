# PR04 Monad Testnet execution evidence

Observed on Monad Testnet (chain ID `10143`) on 2026-09-24. This is a **test-only** deployment using dUSD and dNVDA demonstration ERC-20s. dNVDA carries no NVIDIA shares or equity rights. It is separate from Monad mainnet products and customer funds.

## Contracts

| Role | Address | Deployment transaction |
| --- | --- | --- |
| dUSD demo cash | `0x7ba06B28D429C36Db285B762F6816CDe3b358946` | [tx](https://testnet.monadscan.com/tx/0x52fa95e0b234c912c3855dea43e5f24ec8a93edb40f5784088d311a6a936e326) |
| dNVDA demo stock | `0x73225Ce4A4499E9a80720d5AEB1C4894f7f993e7` | [tx](https://testnet.monadscan.com/tx/0xcbe9b24d05bf85809d6a9fc04396100f629f0867e1c2527c9b40501d76f73be6) |
| StockMesh executor | `0x6E1Cbf7A639819f51Db3C087e1c5c0745D54FA05` | [tx](https://testnet.monadscan.com/tx/0x9ac9ac7a311043169f8d0e5719a1e1b2ec30f33fda89f8f331b6953e1640b58f) |
| Venue A | `0x1aAC2eb923BaF16B3bb69FdEDD13ae1Ce443B580` | [tx](https://testnet.monadscan.com/tx/0xa80440ff8acc62a4ffd3dd36755d8c2341f2474653426bc14c767723270f345a) |
| Venue B | `0x49801A2E790Ec73249Bd03090Bc11a5aaDb8f4e0` | [tx](https://testnet.monadscan.com/tx/0x6fdad49dc77f471bda6601755893374897b90a034af77e730993de503f1e55b0) |

The contracts were configured and both venues seeded with test liquidity. The isolated deployer was `0xAF754A79790f462347D819914b43415A761B52eC`. The test buyer was `0x2e85100eB9bfb931948297A62FE044B3AD419e85`.

## Signed transaction sequence

| Action | Outcome | Transaction |
| --- | --- | --- |
| Claim 1,000 dUSD | confirmed | [tx](https://testnet.monadscan.com/tx/0x1b9e8aca51cde7b5d99e36fd4e2d01a2ff890f064ff855585e6156ba567e2e55) |
| Approve 100 dUSD | confirmed | [tx](https://testnet.monadscan.com/tx/0x87aed3d89db05a4c4ffec892a8513136c54dd64c70334e6567824306156e4ec2) |
| Buy dNVDA through Venue A | confirmed, 995,957 base units received | [tx](https://testnet.monadscan.com/tx/0xd6bae1478f437a75bed7ffdc4cd3057d6e7cf4aeb17a6eab0c52e34044778ae5) |
| Slippage-floor violation | reverted (`status=0`), input/output balances unchanged, nonce unused | [tx](https://testnet.monadscan.com/tx/0xfa641de843ec15b2a6a7e6d67784c84a3297304c3b50ef79a8cccacbee44ba90) |
| Sell half through Venue B | confirmed, 59,485,850 dUSD base units returned | [tx](https://testnet.monadscan.com/tx/0x436a9437436b88741e64862962f15daaefcb5bf8177737cd8f580f1db8039b1e) |

The smoke runner compared quotes from both deployed venues for each side and selected Venue A for buy and Venue B for sell. Its receipt/event assertions checked exact buyer token balance deltas, 0.5 bps buy fee, used nonce, duplicate-order rejection, failed-order rollback, and sell-side cash return. The post-run buyer balances were 959,485,850 dUSD and 497,979 dNVDA base units. The on-chain buy/sell receipts—not unit tests or fixture output—are the testnet execution evidence.

## Browser wallet-interface end-to-end

A separate headless Chromium run used an isolated EIP-1193 test signer backed by the funded test-only buyer wallet. It exercised the deployed Next.js trading page—not only the contract script—against Monad Testnet:

1. Connect wallet, load dUSD/dNVDA balances, compare both venue quotes, review, simulate, and sign a [10 dUSD buy](https://testnet.monadscan.com/tx/0x14a704a0ec61c7926f68d4a926a51235e9027faf2fdb2d1da3db8ea8e17ebe1d).
2. Reload before checking the receipt, reconnect, recover the pending hash from local storage, confirm the executor event, and update holdings. Cash changed from 949,485,850 to 939,485,850 base units; stock from 597,465 to 696,931.
3. Review and sign a [0.1 dNVDA sell](https://testnet.monadscan.com/tx/0xf14087aca79ee5e515090d73fc35b6511d891e68ae390cc95169d022b6c6f9bc).
4. Reload before receipt confirmation again; reconnect and recover the pending sell. Cash rose to 951,424,194 base units and stock fell to 596,931.

The isolated buyer was replenished with [0.5 testnet MON](https://testnet.monadscan.com/tx/0x06d01ad33974c3bf7ae9abdd033c9ee1685ecf38ac414a8a2c9540912814f4ff) before this run. The screenshot and full JSON evidence are retained on the isolated Cherry host at `/srv/skew/stockmesh-direct-node-20260920/runtime/pr04-ui-e2e/`.

A [Vercel preview](https://skew-deals-eug3a6nrn-woon20020501-pixels-projects.vercel.app/exchange/monad/testnet) is deployed and READY; same-project authenticated `vercel curl` returned HTTP 200 and the exact testnet contract addresses. The preview is protected by Vercel login. The browser automation ran the same build from an isolated Cherry server, not the protected Vercel URL. A personally unlocked MetaMask extension was **not** used; the EIP-1193 test signer is explicitly a test harness.

## Scope boundary

The `HACKATHON_TESTNET_ACCEPTED` **demo execution and recovery** gate is met by real chain receipts, two venues, and the browser wallet-interface test. This does **not** establish a mainnet issuer-authorized stock route, 112 executable equities, equity ownership/redemption rights, production customer settlement, or a public production release. Mainnet stock admission remains closed.
