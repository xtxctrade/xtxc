const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const solc = require('solc');
const ganache = require('ganache');
const { ethers } = require('ethers');

const root = path.join(__dirname, '..');
const input = {
  language: 'Solidity',
  sources: {
    'StockMeshExecutor.sol': { content: fs.readFileSync(path.join(root, 'src/StockMeshExecutor.sol'), 'utf8') },
    'MockAtomicVenue.sol': { content: fs.readFileSync(path.join(__dirname, 'MockAtomicVenue.sol'), 'utf8') },
  },
  settings: { evmVersion: 'shanghai', viaIR: true, optimizer: { enabled: true, runs: 200 }, outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object'] } } },
};
const compiled = JSON.parse(solc.compile(JSON.stringify(input)));
const errors = (compiled.errors || []).filter(e => e.severity === 'error');
assert.deepEqual(errors, [], errors.map(e => e.formattedMessage).join('\n'));

function factory(file, name, signer) {
  const c = compiled.contracts[file][name];
  return new ethers.ContractFactory(c.abi, `0x${c.evm.bytecode.object}`, signer);
}
async function rejected(promise, label) {
  let failed = false;
  try { const tx = await promise; await tx.wait(); } catch { failed = true; }
  assert.equal(failed, true, label);
}

(async () => {
  const chain = ganache.provider({ chain: { chainId: 143 }, logging: { quiet: true }, wallet: { totalAccounts: 4 } });
  const provider = new ethers.BrowserProvider(chain);
  const governor = await provider.getSigner(0);
  const user = await provider.getSigner(1);
  const fees = await provider.getSigner(2);
  const attacker = await provider.getSigner(3);
  const usdc = await factory('MockAtomicVenue.sol', 'MockToken', governor).deploy('USD Coin', 'USDC');
  const stock = await factory('MockAtomicVenue.sol', 'MockToken', governor).deploy('Stock', 'STOCK');
  const venue = await factory('MockAtomicVenue.sol', 'MockAtomicVenue', governor).deploy();
  await Promise.all([usdc.waitForDeployment(), stock.waitForDeployment(), venue.waitForDeployment()]);
  const executor = await factory('StockMeshExecutor.sol', 'StockMeshExecutor', governor).deploy(await usdc.getAddress(), await fees.getAddress());
  await executor.waitForDeployment();
  const productId = ethers.id('eip155:143:erc20:stock:issuer:product');
  await (await executor.configureProduct(productId, await stock.getAddress())).wait();
  await (await executor.configureVenue(productId, await venue.getAddress(), true)).wait();
  await (await usdc.mint(await user.getAddress(), 5_000_000n)).wait();
  await (await stock.mint(await venue.getAddress(), 5_000_000n)).wait();
  await (await usdc.mint(await venue.getAddress(), 5_000_000n)).wait();
  await (await usdc.connect(user).approve(await executor.getAddress(), 5_000_000n)).wait();

  const base = {
    productId, quoteDigest: ethers.id('quote'), owner: await user.getAddress(),
    receiver: await user.getAddress(), stock: await stock.getAddress(), venue: await venue.getAddress(),
    nonce: 1n, amountIn: 1_000_000n, maxDebit: 1_000_000n,
    minOutput: 999_950n, feeCap: 50n, deadline: 4_000_000_000n, buy: true,
  };
  const buy = executor.connect(user);
  const before = await stock.balanceOf(await user.getAddress());
  await (await buy.execute(base)).wait();
  assert.equal((await stock.balanceOf(await user.getAddress())) - before, 999_950n);
  assert.equal(await usdc.balanceOf(await fees.getAddress()), 50n);
  assert.equal(await executor.nonceUsed(await user.getAddress(), 1n), true);
  await rejected(buy.execute(base), 'reused nonce');
  await rejected(executor.connect(attacker).execute({ ...base, nonce: 2n }), 'wrong owner');
  await rejected(buy.execute({ ...base, nonce: 2n, receiver: await attacker.getAddress() }), 'wrong receiver');
  await rejected(buy.execute({ ...base, nonce: 2n, minOutput: 1_000_000n }), 'min output');
  await rejected(buy.execute({ ...base, nonce: 2n, feeCap: 49n }), 'fee cap');
  await rejected(buy.execute({ ...base, nonce: 2n, maxDebit: 999_999n }), 'max debit');
  await rejected(buy.execute({ ...base, nonce: 2n, venue: await attacker.getAddress() }), 'unadmitted venue');
  assert.equal(await executor.nonceUsed(await user.getAddress(), 2n), false);

  // Pre-existing dust cannot satisfy the new order's minimum output.
  await (await stock.mint(await executor.getAddress(), 500n)).wait();
  await rejected(buy.execute({ ...base, nonce: 2n, minOutput: 1_000_100n }), 'dust isolation');
  assert.equal(await stock.balanceOf(await executor.getAddress()), 500n);
  await (await venue.setMode(true, false, false)).wait();
  await rejected(buy.execute({ ...base, nonce: 2n }), 'partial input spend');
  await (await venue.setMode(false, true, false)).wait();
  await rejected(buy.execute({ ...base, nonce: 2n }), 'wrong delivery receiver');
  await (await venue.setMode(false, false, true)).wait();
  await rejected(buy.execute({ ...base, nonce: 2n }), 'venue revert');
  await (await venue.setMode(false, false, false)).wait();
  assert.equal(await executor.nonceUsed(await user.getAddress(), 2n), false);

  await (await stock.connect(user).approve(await executor.getAddress(), 200_000n)).wait();
  const sell = { ...base, nonce: 3n, amountIn: 200_000n, maxDebit: 200_000n,
    minOutput: 199_990n, feeCap: 10n, buy: false };
  const usdcBefore = await usdc.balanceOf(await user.getAddress());
  await (await buy.execute(sell)).wait();
  assert.equal((await usdc.balanceOf(await user.getAddress())) - usdcBefore, 199_990n);
  assert.equal(await usdc.balanceOf(await fees.getAddress()), 60n);
  assert.equal(await stock.balanceOf(await executor.getAddress()), 500n);

  // Changing the product stock invalidates old venue admissions.
  const replacement = await factory('MockAtomicVenue.sol', 'MockToken', governor).deploy('Replacement', 'R');
  await replacement.waitForDeployment();
  await (await executor.configureProduct(productId, await replacement.getAddress())).wait();
  await rejected(buy.execute({ ...base, stock: await replacement.getAddress(), nonce: 4n }), 'stale venue admission');
  assert.equal(await executor.nonceUsed(await user.getAddress(), 4n), false);
  await chain.disconnect();
  console.log('StockMeshExecutor: buy/sell, nonce, owner, receiver, debit, fee, dust, partial, failure, stale admission PASS');
})().catch(e => { console.error(e); process.exitCode = 1; });
