const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const ganache = require('ganache');
const { ethers } = require('ethers');

const root = path.join(__dirname, '..');
const files = ['ETFVaultShare.sol', 'ETFFactory.sol', 'ETFPositionFlow.sol', 'MetropolisDemoAssets.sol'];
const sources = Object.fromEntries(files.map(name => [name, {
  content: fs.readFileSync(path.join(root, 'src', name), 'utf8'),
}]));
const result = JSON.parse(solc.compile(JSON.stringify({ language: 'Solidity', sources,
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } } })));
const errors = (result.errors || []).filter(e => e.severity === 'error');
assert.deepEqual(errors, [], errors.map(e => e.formattedMessage).join('\n'));

function deploy(name, signer) {
  const file = name === 'MetropolisDemoToken' || name === 'MetropolisDemoVenue'
    ? 'MetropolisDemoAssets.sol' : `${name}.sol`;
  const artifact = result.contracts[file][name];
  return new ethers.ContractFactory(artifact.abi, `0x${artifact.evm.bytecode.object}`, signer);
}
async function wait(tx) { return (await tx).wait(); }
async function rejected(promise, label) {
  let failed = false;
  try { await wait(promise); } catch { failed = true; }
  assert.equal(failed, true, label);
}

(async () => {
  const chain = ganache.provider({ chain: { chainId: 10143 },
    miner: { blockGasLimit: 30_000_000 }, logging: { quiet: true },
    wallet: { totalAccounts: 5 } });
  const provider = new ethers.BrowserProvider(chain);
  const governor = await provider.getSigner(0);
  const alice = await provider.getSigner(1);
  const bob = await provider.getSigner(2);
  const feeRecipient = await provider.getSigner(3);
  const usdc = await deploy('MetropolisDemoToken', governor).deploy('Demo USD', 'dUSD', false);
  const a = await deploy('MetropolisDemoToken', governor).deploy('Demo A', 'dA', false);
  const b = await deploy('MetropolisDemoToken', governor).deploy('Demo B', 'dB', false);
  await Promise.all([usdc.waitForDeployment(), a.waitForDeployment(), b.waitForDeployment()]);
  const factory = await deploy('ETFFactory', governor).deploy(await governor.getAddress());
  await factory.waitForDeployment();
  for (const token of [a, b]) await wait(factory.configureAsset(await token.getAddress(), true));
  const factoryAddress = await factory.getAddress();
  const units = [1_000_000n, 1_000_000n];
  const assets = [await a.getAddress(), await b.getAddress()];
  const share = 10n ** 18n;
  const metadata = ethers.id('pr06-fixed-unit-demo');
  const digest = await factory.definitionDigest(await governor.getAddress(), 'Demo ETF', 'dETF',
    1n, metadata, assets, units, 10n ** 12n);
  await wait(factory.createETF('Demo ETF', 'dETF', 1n, 6n, metadata, assets, units, 10n ** 12n, digest));
  const vaultAddress = await factory.vaultByCreatorNonce(await governor.getAddress(), 6n);
  const vault = new ethers.Contract(vaultAddress,
    result.contracts['ETFVaultShare.sol'].ETFVaultShare.abi, provider);
  const venueA = await deploy('MetropolisDemoVenue', governor).deploy(await usdc.getAddress(), assets[0], 30n);
  const venueB = await deploy('MetropolisDemoVenue', governor).deploy(await usdc.getAddress(), assets[1], 30n);
  await Promise.all([venueA.waitForDeployment(), venueB.waitForDeployment()]);
  for (const token of [a, b]) await wait(token.mint(await governor.getAddress(), 10_000_000_000n));
  await wait(usdc.mint(await governor.getAddress(), 200_000_000_000n));
  await wait(usdc.mint(await alice.getAddress(), 50_000_000n));
  await wait(usdc.mint(await bob.getAddress(), 20_000_000n));
  for (const [token, venue] of [[a, venueA], [b, venueB]]) {
    await wait(usdc.approve(await venue.getAddress(), 100_000_000_000n));
    await wait(token.approve(await venue.getAddress(), 5_000_000_000n));
    await wait(venue.addLiquidity(100_000_000_000n, 5_000_000_000n));
  }
  const flow = await deploy('ETFPositionFlow', governor).deploy(
    await usdc.getAddress(), factoryAddress, await feeRecipient.getAddress());
  await flow.waitForDeployment();
  const flowAddress = await flow.getAddress();
  await wait(flow.configureVault(vaultAddress, true));
  await wait(flow.configureVenue(vaultAddress, assets[0], await venueA.getAddress(), true));
  await wait(flow.configureVenue(vaultAddress, assets[1], await venueB.getAddress(), true));
  await wait(usdc.connect(alice).approve(flowAddress, 50_000_000n));
  await wait(usdc.connect(bob).approve(flowAddress, 20_000_000n));
  const deadline = BigInt(Math.floor(Date.now() / 1000) + 3600);
  const cashLegs = () => [
    { fromWallet: 0n, cashIn: 24_000_000n, minBought: 1_000_000n, venue: venueA.target },
    { fromWallet: 0n, cashIn: 24_000_000n, minBought: 1_000_000n, venue: venueB.target },
  ];
  // Pool ratio is 20:1, so one share of each asset costs roughly $40.
  const first = { vault: vaultAddress, owner: await alice.getAddress(), receiver: await alice.getAddress(),
    shares: share, nonce: 1n, quoteDigest: ethers.id('cash-only'), deadline,
    maxUsdcDebit: 50_000_000n, feeCap: 2_500n, legs: cashLegs() };
  const firstFeeBefore = await usdc.balanceOf(await feeRecipient.getAddress());
  const firstReceipt = await wait(flow.connect(alice).invest(first));
  assert.equal(await vault.balanceOf(await alice.getAddress()), share);
  assert.equal(await usdc.balanceOf(await feeRecipient.getAddress()) - firstFeeBefore, 2_400n);
  assert.equal(await usdc.balanceOf(await alice.getAddress()), 1_997_600n);
  assert.equal(await flow.nonceUsed(await alice.getAddress(), 1n), true);
  await rejected(flow.connect(alice).invest(first), 'nonce replay');
  await wait(vault.connect(alice).approve(flowAddress, share));

  // In-kind exit is available even without any venue; direct user receipt.
  const kindReceipt = await wait(flow.connect(alice).redeemInKind(vaultAddress, share / 2n,
    2n, ethers.id('kind'), deadline));
  assert.equal(await vault.balanceOf(await alice.getAddress()), share / 2n);
  assert.ok(await a.balanceOf(await alice.getAddress()) >= 500_000n);
  assert.ok(await b.balanceOf(await alice.getAddress()) >= 500_000n);
  await rejected(flow.connect(bob).redeemToUsdc({ vault: vaultAddress,
    owner: await bob.getAddress(), receiver: await alice.getAddress(), shares: share / 2n,
    nonce: 1n, quoteDigest: ethers.id('wrong receiver'), deadline,
    minUsdcOut: 1n, feeCap: 1_000n, legs: [
      { venue: venueA.target, minUsdcOut: 1n }, { venue: venueB.target, minUsdcOut: 1n },
    ] }), 'wrong receiver');

  // Partial existing holdings are used only when explicitly listed in the order.
  await wait(usdc.mint(await alice.getAddress(), 30_000_000n));
  await wait(usdc.connect(alice).approve(flowAddress, 30_000_000n));
  await wait(a.connect(alice).approve(flowAddress, 500_000n));
  await wait(b.connect(alice).approve(flowAddress, 500_000n));
  const partial = { ...first, nonce: 3n, quoteDigest: ethers.id('partial'),
    maxUsdcDebit: 30_000_000n, feeCap: 1_500n, legs: [
      { fromWallet: 500_000n, cashIn: 14_000_000n, minBought: 500_000n, venue: venueA.target },
      { fromWallet: 500_000n, cashIn: 14_000_000n, minBought: 500_000n, venue: venueB.target },
    ] };
  const partialReceipt = await wait(flow.connect(alice).invest(partial));
  assert.equal(await vault.balanceOf(await alice.getAddress()), share * 3n / 2n);
  assert.equal(await a.balanceOf(await alice.getAddress()) > 0n, true, 'surplus returned');
  await rejected(flow.connect(alice).invest({ ...partial, nonce: 4n, shares: 1n }), 'dust shares');
  await rejected(flow.connect(alice).invest({ ...partial, nonce: 4n, receiver: await bob.getAddress() }),
    'wrong receiver invest');
  await rejected(flow.connect(alice).invest({ ...partial, nonce: 4n, feeCap: 0n }), 'fee cap');
  const supplyBeforeFailure = await vault.totalSupply();
  const aliceCashBeforeFailure = await usdc.balanceOf(await alice.getAddress());
  await rejected(flow.connect(alice).invest({ ...partial, nonce: 4n,
    legs: [partial.legs[0], { ...partial.legs[1], minBought: 999_999_999n }] }),
  'late-leg price moved');
  assert.equal(await vault.totalSupply(), supplyBeforeFailure);
  assert.equal(await usdc.balanceOf(await alice.getAddress()), aliceCashBeforeFailure);
  await rejected(flow.connect(alice).invest({ ...first, nonce: 4n, maxUsdcDebit: 40_000_000n }),
    'insufficient wallet cash');
  assert.equal(await flow.nonceUsed(await alice.getAddress(), 4n), false, 'failed order rolled back');

  // A fully funded wallet issues without a venue, USDC allowance or fee.
  for (const token of [a, b]) {
    await wait(token.mint(await bob.getAddress(), 1_000_000n));
    await wait(token.connect(bob).approve(flowAddress, 1_000_000n));
  }
  await wait(usdc.connect(bob).approve(flowAddress, 0n));
  const inKindBuy = { vault: vaultAddress, owner: await bob.getAddress(),
    receiver: await bob.getAddress(), shares: share, nonce: 1n,
    quoteDigest: ethers.id('wallet-only'), deadline, maxUsdcDebit: 0n, feeCap: 0n,
    legs: assets.map(() => ({ fromWallet: 1_000_000n, cashIn: 0n,
      minBought: 0n, venue: ethers.ZeroAddress })) };
  await wait(flow.connect(bob).invest(inKindBuy));
  assert.equal(await vault.balanceOf(await bob.getAddress()), share);
  await wait(vault.connect(bob).approve(flowAddress, share));
  await wait(flow.connect(bob).redeemInKind(vaultAddress, share, 2n,
    ethers.id('wallet-only-exit'), deadline));
  assert.equal(await vault.balanceOf(await bob.getAddress()), 0n);

  // An ETF transferred from another wallet can still be exited by its new owner.
  await wait(vault.connect(alice).transfer(await bob.getAddress(), share / 2n));
  await wait(vault.connect(bob).approve(flowAddress, share / 2n));
  await wait(factory.configureAsset(assets[0], false));
  const exit = { vault: vaultAddress, owner: await bob.getAddress(), receiver: await bob.getAddress(),
    shares: share / 2n, nonce: 5n, quoteDigest: ethers.id('cash-exit'), deadline,
    minUsdcOut: 1n, feeCap: 2_000_000n, legs: [
      { venue: venueA.target, minUsdcOut: 1n }, { venue: venueB.target, minUsdcOut: 1n },
    ] };
  const beforeBobCash = await usdc.balanceOf(await bob.getAddress());
  const exitReceipt = await wait(flow.connect(bob).redeemToUsdc(exit));
  assert.ok(await usdc.balanceOf(await bob.getAddress()) > beforeBobCash);
  assert.equal(await vault.balanceOf(await bob.getAddress()), 0n);
  assert.equal(await flow.nonceUsed(await bob.getAddress(), 5n), true);
  await rejected(flow.connect(bob).redeemToUsdc(exit), 'exit replay');
  await wait(vault.connect(alice).approve(flowAddress, 0n));
  const cannotExit = { ...exit, owner: await alice.getAddress(), receiver: await alice.getAddress(),
    nonce: 6n, shares: share / 2n };
  await rejected(flow.connect(alice).redeemToUsdc(cannotExit), 'wrong allowance');
  assert.equal(await vault.balanceOf(await alice.getAddress()), share);
  await wait(vault.connect(alice).approve(flowAddress, share));
  await rejected(flow.connect(alice).redeemToUsdc({ ...cannotExit, minUsdcOut: 10n ** 18n }),
    'cash floor failure rolls back redemption');
  assert.equal(await vault.balanceOf(await alice.getAddress()), share);
  await wait(flow.connect(alice).redeemToUsdc({ ...cannotExit, shares: share, nonce: 7n }));
  assert.equal(await vault.totalSupply(), 0n);
  assert.equal(await a.balanceOf(vaultAddress), 0n);
  assert.equal(await b.balanceOf(vaultAddress), 0n);
  await wait(factory.configureAsset(assets[0], true));

  await wait(flow.configureVenue(vaultAddress, assets[0], await venueA.getAddress(), false));
  await rejected(flow.connect(bob).invest({ ...first, owner: await bob.getAddress(),
    receiver: await bob.getAddress(), nonce: 8n }), 'venue revocation');
  await chain.disconnect();
  console.log(`ETF position flow: cash, partial holdings, in-kind/cash exit, transfer, `
    + `fee/refund, allowance, price movement, replay, rollback PASS `
    + `gas invest=${firstReceipt.gasUsed}/${partialReceipt.gasUsed} `
    + `exit=${kindReceipt.gasUsed}/${exitReceipt.gasUsed}`);
})().catch(e => { console.error(e); process.exitCode = 1; });
