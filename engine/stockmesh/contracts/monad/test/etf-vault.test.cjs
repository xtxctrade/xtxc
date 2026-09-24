const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const ganache = require('ganache');
const { ethers } = require('ethers');

const root = path.join(__dirname, '..');
const names = ['ETFVaultShare.sol', 'ETFFactory.sol', 'ETFMockAsset.sol'];
const sources = Object.fromEntries(names.map(name => [name, { content: fs.readFileSync(
  path.join(root, name === 'ETFMockAsset.sol' ? 'test' : 'src', name), 'utf8') }]));
const compiled = JSON.parse(solc.compile(JSON.stringify({ language: 'Solidity', sources,
  settings: { evmVersion: 'shanghai', viaIR: true,
    optimizer: { enabled: true, runs: 200 },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } } })));
const errors = (compiled.errors || []).filter(e => e.severity === 'error');
assert.deepEqual(errors, [], errors.map(e => e.formattedMessage).join('\n'));

function make(name, signer) {
  const file = name === 'ETFMockAsset' ? 'ETFMockAsset.sol'
    : name === 'ETFFactory' ? 'ETFFactory.sol' : 'ETFVaultShare.sol';
  const c = compiled.contracts[file][name];
  return new ethers.ContractFactory(c.abi, `0x${c.evm.bytecode.object}`, signer);
}
async function rejected(promise, label) {
  let failed = false;
  try { const tx = await promise; await tx.wait(); } catch { failed = true; }
  assert.equal(failed, true, label);
}

(async () => {
  const chain = ganache.provider({ chain: { chainId: 10143, allowUnlimitedContractSize: false },
    miner: { blockGasLimit: 30_000_000 }, logging: { quiet: true }, wallet: { totalAccounts: 4 } });
  const provider = new ethers.BrowserProvider(chain);
  const governor = await provider.getSigner(0);
  const alice = await provider.getSigner(1);
  const bob = await provider.getSigner(2);
  const guardian = await provider.getSigner(3);
  const factory = await make('ETFFactory', governor).deploy(await guardian.getAddress());
  await factory.waitForDeployment();
  const tokens = [];
  for (let i = 0; i < 16; i++) {
    const token = await make('ETFMockAsset', governor).deploy(`Demo ${i}`, `d${i}`);
    await token.waitForDeployment();
    tokens.push(token);
    await (await factory.configureAsset(await token.getAddress(), true)).wait();
    await (await token.mint(await alice.getAddress(), 10_000_000n)).wait();
  }
  const addresses = await Promise.all(tokens.map(t => t.getAddress()));
  const scale = 10n ** 18n;
  const granularity = 10n ** 12n;
  let maxMintGas = 0n;
  let maxRedeemGas = 0n;
  for (const count of [2, 3, 8, 16]) {
    const assets = addresses.slice(0, count);
    const units = Array(count).fill(1_000_000n);
    const label = `ETF ${count}`;
    const digest = await factory.definitionDigest(await alice.getAddress(), label,
      `E${count}`, 1n, ethers.id(`metadata-${count}`), assets, units, granularity);
    const tx = await factory.connect(alice).createETF(label, `E${count}`, 1n, count,
      ethers.id(`metadata-${count}`), assets, units, granularity, digest);
    const receipt = await tx.wait();
    const created = receipt.logs.map(l => { try { return factory.interface.parseLog(l); } catch { return null; } })
      .find(l => l?.name === 'ETFCreated');
    assert.ok(created, `create event ${count}`);
    const address = created.args.vault;
    const vault = new ethers.Contract(address, compiled.contracts['ETFVaultShare.sol'].ETFVaultShare.abi, provider);
    assert.equal(await factory.vaultByCreatorNonce(await alice.getAddress(), BigInt(count)), address);
    assert.equal(await vault.definitionHash(), digest);
    assert.equal(await vault.assetCount(), BigInt(count));
    for (const t of tokens.slice(0, count)) {
      await (await t.connect(alice).approve(address, 10_000_000n)).wait();
    }
    assert.deepEqual(Array.from(await vault.previewClaim(2n * scale)), Array(count).fill(2_000_000n));
    const issued = await (await vault.connect(alice).mint(2n * scale, await alice.getAddress())).wait();
    if (issued.gasUsed > maxMintGas) maxMintGas = issued.gasUsed;
    assert.equal(await vault.totalSupply(), 2n * scale);
    for (let i = 0; i < count; i++) {
      const [held, owed, surplus] = await vault.reserveAt(i);
      assert.deepEqual([held, owed, surplus], [2_000_000n, 2_000_000n, 0n]);
    }
    await rejected(vault.connect(alice).mint(1n, await alice.getAddress()), 'granularity');
    await rejected(vault.connect(alice).transfer(await bob.getAddress(), 1n), 'dust transfer');
    await (await vault.connect(alice).transfer(await bob.getAddress(), scale / 2n)).wait();
    await (await vault.connect(alice).approve(await bob.getAddress(), scale / 2n)).wait();
    await (await vault.connect(bob).transferFrom(await alice.getAddress(),
      await bob.getAddress(), scale / 2n)).wait();
    assert.equal(await vault.balanceOf(await bob.getAddress()), scale);
    await (await factory.connect(guardian).pauseIssuance(address)).wait();
    await rejected(vault.connect(alice).mint(scale, await alice.getAddress()), 'pause mint');
    await rejected(factory.connect(guardian).resumeIssuance(address), 'guardian cannot resume');
    const redeemed = await (await vault.connect(bob).redeem(scale / 2n, await bob.getAddress())).wait();
    if (redeemed.gasUsed > maxRedeemGas) maxRedeemGas = redeemed.gasUsed;
    assert.equal(await vault.balanceOf(await bob.getAddress()), scale / 2n);
    await (await vault.connect(bob).redeem(scale / 2n, await bob.getAddress())).wait();
    await (await vault.connect(alice).redeem(scale, await alice.getAddress())).wait();
    assert.equal(await vault.totalSupply(), 0n);
    for (let i = 0; i < count; i++) {
      const [held, owed, surplus] = await vault.reserveAt(i);
      assert.deepEqual([held, owed, surplus], [0n, 0n, 0n]);
    }
    await rejected(factory.connect(alice).createETF(label, `E${count}`, 1n, count,
      ethers.id(`metadata-${count}`), assets, units, granularity, digest), 'nonce replay');
    await rejected(factory.connect(alice).createETF(label, `E${count}`, 1n, count + 100,
      ethers.id(`metadata-${count}`), assets, units, granularity, digest), 'definition replay');
    await rejected(factory.connect(bob).createETF(label, `E${count}`, 1n, count + 200,
      ethers.id(`metadata-${count}`), assets, units, granularity, digest),
    'creator-bound digest blocks copied transaction');
  }

  // Donation is not share issuance or additional redemption entitlement.
  const two = addresses.slice(0, 2);
  const units = [1_000_000n, 1_000_000n];
  const digest = await factory.definitionDigest(await alice.getAddress(), 'Donation', 'DON', 1n,
    ethers.id('donation'), two, units, granularity);
  const create = await (await factory.connect(alice).createETF('Donation', 'DON', 1n, 55n,
    ethers.id('donation'), two, units, granularity, digest)).wait();
  const event = create.logs.map(l => { try { return factory.interface.parseLog(l); } catch { return null; } })
    .find(l => l?.name === 'ETFCreated');
  const address = event.args.vault;
  const vault = new ethers.Contract(address, compiled.contracts['ETFVaultShare.sol'].ETFVaultShare.abi, provider);
  for (const t of tokens.slice(0, 2)) await (await t.connect(alice).approve(address, 10_000_000n)).wait();
  await (await vault.connect(alice).mint(scale, await alice.getAddress())).wait();
  await (await tokens[0].connect(alice).transfer(address, 10_000n)).wait();
  assert.deepEqual(Array.from(await vault.reserveAt(0)), [1_010_000n, 1_000_000n, 10_000n]);
  await (await vault.connect(alice).redeem(scale, await alice.getAddress())).wait();
  assert.deepEqual(Array.from(await vault.reserveAt(0)), [10_000n, 0n, 10_000n]);
  assert.equal(await vault.totalSupply(), 0n);

  // A later token's failure reverts earlier transfers and share burning.
  await (await vault.connect(alice).mint(scale, await alice.getAddress())).wait();
  await (await tokens[1].configure(0n, true, false, ethers.ZeroAddress)).wait();
  const firstVaultBefore = await tokens[0].balanceOf(address);
  const firstAliceBefore = await tokens[0].balanceOf(await alice.getAddress());
  await rejected(vault.connect(alice).redeem(scale, await alice.getAddress()), 'late-leg revert');
  assert.equal(await vault.totalSupply(), scale);
  assert.equal(await vault.balanceOf(await alice.getAddress()), scale);
  assert.equal(await tokens[0].balanceOf(address), firstVaultBefore);
  assert.equal(await tokens[0].balanceOf(await alice.getAddress()), firstAliceBefore);
  await (await tokens[1].configure(0n, false, false, ethers.ZeroAddress)).wait();

  // Fee-on-transfer, reentry and backing deficit cannot issue unbacked shares.
  await (await tokens[1].configure(100n, false, false, ethers.ZeroAddress)).wait();
  await rejected(vault.connect(alice).mint(scale, await alice.getAddress()), 'fee input');
  assert.equal(await vault.totalSupply(), scale);
  await (await tokens[1].configure(0n, false, false, address)).wait();
  await rejected(vault.connect(alice).mint(scale, await alice.getAddress()), 'reentrant input');
  await (await tokens[1].configure(0n, false, false, ethers.ZeroAddress)).wait();
  await (await tokens[0].burn(address, 10_001n)).wait();
  await rejected(vault.connect(alice).mint(scale, await alice.getAddress()), 'backing deficit');
  await rejected(vault.connect(alice).redeem(scale, await alice.getAddress()), 'deficit redeem');
  assert.equal(await vault.totalSupply(), scale);

  await rejected(factory.connect(alice).createETF('Bad', 'BAD', 1n, 56n,
    ethers.id('bad'), [two[0], two[0]], units, granularity,
    await factory.definitionDigest(await alice.getAddress(), 'Bad', 'BAD', 1n,
      ethers.id('bad'), [two[0], two[0]], units, granularity)), 'duplicate asset');
  const rogue = await make('ETFMockAsset', governor).deploy('Unadmitted', 'NO');
  await rogue.waitForDeployment();
  const rogueAssets = [two[0], await rogue.getAddress()];
  await rejected(factory.connect(alice).createETF('Rogue', 'ROGUE', 1n, 57n,
    ethers.id('rogue'), rogueAssets, units, granularity,
    await factory.definitionDigest(await alice.getAddress(), 'Rogue', 'ROGUE', 1n,
      ethers.id('rogue'), rogueAssets, units, granularity)), 'unadmitted asset');
  await rejected(make('ETFVaultShare', alice).deploy(await factory.getAddress(),
    await alice.getAddress(), 'Direct', 'DIR', 1n, ethers.id('direct'), two, units,
    granularity), 'direct vault deployment cannot impersonate factory');
  await rejected(vault.connect(bob).setIssuancePaused(true), 'nonfactory cannot pause');
  await chain.disconnect();
  assert.ok(maxMintGas < 12_000_000n && maxRedeemGas < 12_000_000n, '16-asset gas bound');
  console.log(`ETF vault: 2/3/8/16 assets, transfer/redeem, donation, pause, replay, failure rollback, reentry, gas mint=${maxMintGas} redeem=${maxRedeemGas} PASS`);
})().catch(e => { console.error(e); process.exitCode = 1; });
